// SPDX-License-Identifier: MPL-2.0
//! Dynamic diffuse GI from GPU-traced irradiance probes (spec 27 § 4.2 stage 2a).
//!
//! Camera-relative cascades of probe volumes (dense near the eye, coarser farther out)
//! scroll with the camera. Every frame a share of the probes cast rays in compute: against
//! the terrain heightfield exactly (a maximum-height pyramid built from the shared draw's
//! heights) and against per-mesh signed-distance bricks. A brick stays while its mesh is
//! drawn and can be evicted after it leaves the frame. A hit is lit
//! by the sun (shadow ray through the same scene), by the previous frame's probes
//! (multi-bounce) and by its emission; a miss sees the sky map. Rays are blended into an
//! octahedral irradiance tile and a distance-moment tile per probe with hysteresis; probes
//! inside geometry move out or switch off. Meshes, scatter and terrain shade their diffuse
//! indirect light from the same probes (`ddgi_sample.wgsl`).
//!
//! ## Provenance
//! Majercik, Guertin, Nowrouzezahrai, McGuire, "Dynamic Diffuse Global Illumination with
//! Ray-Traced Irradiance Fields", JCGT 8(2) 2019: probe grid, octahedral irradiance and
//! distance moments, Chebyshev visibility, hysteresis, multi-bounce through the probes.
//! Majercik, Marrs, Spjut, McGuire, "Scaling Probe-Based Real-Time Dynamic Global
//! Illumination for Production", JCGT 10(2) 2021: camera-relative scrolling volumes in
//! cascades, probe relocation and classification. Tevs, Ihrke, Seidel, "Maximum Mipmaps
//! for Fast, Accurate, and Scalable Dynamic Height Field Rendering", I3D 2008: heightfield
//! traversal. Hart, "Sphere Tracing", 1996: brick traversal. Shoemake, "Uniform Random
//! Rotations", Graphics Gems III 1992: the per-frame ray rotation.

use std::collections::{HashMap, HashSet};

use regia_core::glam::{Mat4, Vec3};

use crate::ddgi_bake::{SdfBrick, bake_brick_oriented, brick_value_count, mean_linear_rgb};
use crate::{FrameMesh, ShadowCast};

/// Bytes of the probes' uniform (`DdgiUniform`), for hosts that bind a stand-in.
pub const DDGI_UNIFORM_BYTES: u64 = size_of::<DdgiUniformGpu>() as u64;

/// Most cascades a volume stack may have.
pub const MAX_CASCADES: usize = 4;
/// Rays per probe update (the shaders' `RAYS`).
pub const RAYS_PER_PROBE: u32 = 64;
const PROBE_WORDS: u64 = 12;
const IRR_WORDS: u64 = 192;
const VIS_WORDS: u64 = 256;
const HEADER_WORDS: u64 = 2;
const PACKED_IRR_WORDS: u64 = 64;
/// Half-resolution screen probe lookup: words per texel (six RGB9E5 irradiances toward the
/// ambient-cube directions, distance to the eye, coverage).
const SCREEN_WORDS: u64 = 8;
/// Largest half-resolution width and height of the screen lookup (a 3840 × 2160 internal
/// frame; larger frames read the probes per pixel).
pub(crate) const SCREEN_MAX: (u32, u32) = (1920, 1080);
/// Signed-distance values the brick pool holds by default (32 MB).
pub const SDF_POOL_VALUES: u64 = 8 << 20;
/// Default brick-pool budget in bytes ([`SDF_POOL_VALUES`] × 4). [`DdgiSettings::sdf_pool_bytes`].
pub const SDF_POOL_BYTES: u64 = SDF_POOL_VALUES * 4;
/// Frames a mesh may go undrawn before its brick is eligible for eviction (~5 s at 60 fps).
pub const SDF_EVICT_FRAMES: u32 = 300;
/// Brick instances uploaded per frame.
pub const MAX_INSTANCES: usize = 1024;
/// Emitters in the probes' emitter list per frame (`DdgiSettings::emitters`); the blend
/// shader's workgroup arrays (`ddgi_blend.wgsl`) hold this many.
pub const MAX_EMITTERS: usize = 256;
/// Emitter clusters one mesh contributes at most (cells along its longest axis).
const EMITTER_CELLS: f32 = 4.0;

/// Probe volume settings.
#[derive(Clone, Debug, PartialEq)]
pub struct DdgiSettings {
    /// Cascades (1..=4). Cascade `c` spaces its probes `spacing · spacing_ratio^c` apart.
    pub cascades: u32,
    /// Probes per axis in every cascade.
    pub dims: [u32; 3],
    /// Finest probe spacing, metres.
    pub spacing: f32,
    /// Spacing growth from one cascade to the next.
    pub spacing_ratio: f32,
    /// A settled probe re-traces every `update_divisor` frames (a new one every frame for
    /// its first sixteen updates, ADR-5506). Clamped to `1..=`[`DDGI_MAX_UPDATE_DIVISOR`] (8192).
    pub update_divisor: u32,
    /// Weight of the previous irradiance in the blend (Majercik et al. use 0.97).
    pub hysteresis: f32,
    /// Shading lookups move along the normal by this many probe spacings.
    pub normal_bias: f32,
    /// … and toward the viewer by this many.
    pub view_bias: f32,
    /// Multiplier on the shaded irradiance.
    pub intensity: f32,
    /// Longest terrain ray, metres.
    pub max_terrain_ray: f32,
    /// A probe closer than this many spacings to a front face moves away from it.
    pub min_frontface: f32,
    /// Distance moments clamp at this many spacings.
    pub visibility_limit: f32,
    /// Most brick instances any one cascade traces.
    pub max_instances_per_cascade: u32,
    /// Meshes smaller than this bounding radius (m) are not traced (pebbles).
    pub min_proxy_radius: f32,
    /// Meshes whose brick voxel would be coarser than this (m, world space, the largest
    /// edge) are not traced. A brick has at most 40 voxels per axis, so a kilometre ground
    /// mesh was voxelised in 40–125 m steps and its surface sat tens of metres off: the
    /// probes near it read as buried and the meshes around them went black. Such a mesh is
    /// left to the sky map's ground, as without probes. 8 m keeps meshes up to ~320 m long.
    /// A plate no thicker than `max_proxy_voxel / 8` (a floor or a studio ground of any size)
    /// is traced whatever its length: its thin axis resolves its surface.
    pub max_proxy_voxel: f32,
    /// Emissive meshes light the probes through an emitter list: clusters of each mesh's
    /// emitting triangles become spheres whose intensity carries the cluster's flux, added
    /// analytically in the probe blend with the probe's distance moments as visibility. No
    /// rays and no brick slots, so a strip under `min_proxy_radius` lights the probes too, and
    /// a mesh in the list does not also emit at ray hits on its brick. `false`: emission only
    /// at ray hits on bricks.
    pub emitters: bool,
    /// Range (m) of a short-range emissive pass that lights every emitter as a local light;
    /// 0 when there is none. The probes then take only the far part of each emitter's light,
    /// E · (1 − w(d)) with w(d) = clamp(1 − (d / range)⁴, 0, 1), the range window of the fitted
    /// emissive lights (`emissive_lights`, `lab_forward.wgsl` `emitter_radiance`): a local light
    /// of the full intensity windowed by w(d) and the probes add up to the emitter once. The
    /// renderer sets a per-mesh range itself for the meshes `emissive_lights` fitted this
    /// frame (`EmissiveCache::mesh_ranges`), which takes precedence over this value.
    pub emitter_local_range: f32,
    /// Bake bricks on the calling thread (tests) instead of the task pool.
    pub bake_on_caller: bool,
    /// Mip of the sky map that escaping rays read. 0, the unfiltered sky: the rays are
    /// randomly rotated each frame, so the probes integrate it themselves (the outdoor
    /// prefilter's rough mips lean toward the zenith and read a blue sky ~15 % dark).
    pub sky_lod: f32,
    /// Heightfield cone footprint: at ray distance t the trace accepts the surface at the
    /// pyramid level about `hf_cone · t` wide (exact triangles nearer than ~1 / `hf_cone`
    /// cells). Smaller is more accurate and slower.
    pub hf_cone: f32,
    /// Coarse heightfield surface: the cell's mean height (`true`) or its max box (`false`,
    /// conservative: over-occludes grazing rays).
    pub hf_mean_surface: bool,
    /// The sun shadow rays from ray hits take their own, finer cone footprint over the mean
    /// height. A grazing shadow ray from a terrain hit runs along the surface it starts on:
    /// against the max boxes of the coarse cells it reported the sunlit ground as shadowed, and
    /// the probes lost the ground's sun bounce (Mars hulls' shaded sides at 0.55× the path
    /// tracer, the dyn_light plate). 0 uses the primary rays' cone and surface.
    pub hf_shadow_cone: f32,
    /// Diagnostic: the screen lookup stores the probes' irradiance for the depth normal in all
    /// six directions (exact for that normal, no per-pixel normal response) to measure the
    /// ambient-cube rebuild's error.
    pub screen_exact_debug: bool,
    /// Diagnostic: the probes update and fill the screen lookup but the forward and terrain
    /// passes do not read it (cost studies: the shading's share of the probes' frame cost).
    pub shading_off_debug: bool,
    /// A settled probe classified inactive (buried in the terrain or a mesh) re-traces this
    /// many times less often than an active one (a power of two, 1 = as often). It shades
    /// nothing; it only has to notice geometry moving off it.
    pub inactive_slowdown: u32,
    /// Signed-distance brick pool, bytes. Default [`SDF_POOL_BYTES`] (32 MB), the budget the
    /// pool had when this field did not exist. Rounded down to a multiple of 4 (one distance
    /// value). A change recreates the pool and the bricks rebake. The pool is one storage
    /// buffer bound whole, so [`Self::clamped`] caps the budget at 128 MiB (the
    /// `max_storage_buffer_binding_size` floor WebGPU guarantees on every backend).
    pub sdf_pool_bytes: u64,
    /// Frames a mesh may go undrawn before its brick is eligible for eviction. Default
    /// [`SDF_EVICT_FRAMES`] (300, about 5 s at 60 fps). A value below 1 acts as 1: a brick is
    /// never dropped on a frame its mesh is drawn. Eligible bricks are freed at the start of
    /// the frame, least-recently-drawn first when a new brick needs the room, so the pool
    /// recycles instead of filling until later meshes are skipped. A mesh drawn every frame
    /// keeps its brick and its offset.
    pub sdf_evict_frames: u32,
    /// How the forward pass reads its indirect specular from the probes (ADR-1103): through the
    /// GGX lobe at every roughness ([`crate::ProbeSpecular::Lobe`], the default) or only in the
    /// rough tail (the earlier look). Takes effect on the next frame; the probes are unchanged.
    pub specular: crate::ProbeSpecular,
    /// A probe that starts over without its own light takes a seed worth this many of its own
    /// updates (ADR-2605, `specs/79` § 9.2), at most 15; 0 (the default) starts it from its first
    /// rays alone. A probe whose slot scrolled to a new cell is seeded from where it stood before:
    /// its neighbor one cell toward the volume's inside, the probe the scroll stepped past. A probe
    /// that leaves geometry (inactive to active) is seeded from the next coarser cascade at its
    /// position. Either falls back to the other, and nothing seeds from a probe that has not traced.
    /// The visibility tile is never seeded. The Low and Medium tiers set 4.
    pub seed_updates: u32,
    /// The shading lookup keeps a point on the finest cascade that holds it wherever a coarser
    /// cascade's visible probe weight there is under three quarters of the finest one's, and caps a
    /// coarser cascade's light at 1.25 times the finest one's where that one sees the point
    /// (ADR-2605): a coarse probe relocated out of a room sees a high ceiling through the roof's back
    /// face only, one inside it gathered its light at a spacing that cannot resolve the roof, and the
    /// blend window above the eye handed that ceiling to them. The probes' own ray read is not
    /// changed. `false` (the default) blends by the window alone. Every tier sets it.
    ///
    /// From ADR-5374 to ADR-5506 this option also selected the lookup every layout has now: the
    /// probes weighted at the surface point, the shading lookup and the probes' own ray read alike,
    /// with [`Self::normal_bias`] and [`Self::view_bias`] moving only the point of the visibility
    /// test (Majercik et al. 2019); the irradiance texels next to a coordinate axis holding that
    /// axis's exact cosine lobe; a surface that lies up to 1.05 spacings behind a probe (a wall
    /// between two probe planes) read from that probe's irradiance at the surface, which the probe
    /// estimates from its own rays and keeps in the axis texels of odd parity
    /// ([`DdgiReadback::irradiance`], ADR-5457); and each face interpolated between the probes in a
    /// square-root space (ADR-5506). With the weights at the biased point a surface took part of
    /// its light from the probes up to 0.4 spacings in front of it (a room lit through a door read
    /// 1.15 times the path tracer).
    pub visibility_blend: bool,
    /// Largest screen lookup the probe buffer reserves, texels (width, height). Default
    /// [`SCREEN_LOOKUP_MAX`] (1920 × 1080, a 3840 × 2160 frame at half resolution, 66 MB). A
    /// smaller region (the Low and Medium tiers) saves memory; a frame whose half-resolution
    /// lookup does not fit it takes a quarter-resolution one instead of none. A change recreates
    /// the probe buffers.
    pub screen_max: [u32; 2],
    /// How a young probe's running average weighs its updates (ADR-2605): the `k`-th of `n` counts
    /// in proportion to `k^(recent_weight − 1)`, 1 to 3. 1 (the default) weighs them alike. A probe's
    /// early updates carry fewer bounces of light (the probes they read were young too), so with
    /// equal weights its multi-bounce light approaches the answer only as `n^−(1 − ρ)` for a bounce
    /// gain ρ; weighing recent updates more makes that `n^−c(1 − ρ)` at `c` = this value, for a
    /// little more noise (the variance of `c` = 3 is 1.8 times that of 1). The visibility tile keeps
    /// equal weights. The Low and Medium tiers set 3.
    pub recent_weight: u32,
    /// Metres the finest cascade holds at full weight over the eye, on a layout that needs the room
    /// rule (ADR-1949): its window keeps 1.8 m under the eye for the floor first, then reaches this
    /// far up, and the rest of its height goes under the eye. A room's ceiling higher than the window
    /// reads a coarser cascade, and the visibility blend only takes it back where the finest
    /// cascade's window still holds it. Default [`FINEST_HEADROOM_M`] (2 m, a 3.5 m ceiling over a
    /// standing eye). A negative or non-finite value acts as the default. The volume is placed by this
    /// value; since ADR-5909 the lookup holds half a cell more of the finest cascade at each end of the
    /// window (its blend margin is a quarter cell on such a layout).
    pub finest_headroom_m: f32,
    /// The lookups at a surface (the shading's screen lookup and the probes' own ray hits) also weigh a
    /// probe by which side of that surface it stands on (ADR-5566, `specs/79` § 2.1): a probe behind the
    /// surface's plane counts a thousandth (its weight fades over 0.15 spacings behind the plane). Its light
    /// for the surface's normal is the back of the wall the point is on, or whatever lies between it and that
    /// plane. The visibility test drops such a probe unless it sees the surface's front past an edge: on 2 m
    /// probes the probe a metre outside a door lit the door wall's inner face with the daylight, and on 1 m
    /// probes the probe just inside a room's opening lit the end faces of the room's walls with the light of
    /// their sunlit inner sides (ADR-5716).
    ///
    /// A lookup in free space (the radiance cache's cells) has no surface and keeps its weights. No ray,
    /// probe, byte or word more. `false` (the default) keeps every lookup as it was. Every tier sets it
    /// (ADR-5716; the Low tier alone until then).
    pub front_weights: bool,
    /// A probe that stands well past the edge of the point's surface, with open air where that surface
    /// would lie behind it (the probe outside a door, under no roof, against a point on the room's
    /// ceiling), has to see the point without doubt: the visibility bound's tail under 0.9 counts as
    /// hidden (ADR-5566's second term; its own switch since ADR-5716). The blend keeps, per probe and axis
    /// direction, how much of the probe's distance lobe ends on a surface within 1.05 spacings behind it
    /// (the cover word, probe record word 11). A probe at the edge (half covered) and a probe that sees the
    /// point cleanly keep their weight.
    ///
    /// No ray, probe or byte more: the option reads one more word per probe in the lookup and writes one
    /// per probe update. `false` (the default) keeps every lookup as it was. The Low and Medium tiers set it:
    /// their 2 m probes stand on both sides of a house wall. On 1 m probes it changed no acceptance scene
    /// (ADR-5716), so High and Ultra leave its lookup time out.
    pub edge_visibility: bool,
    /// Most young probes (those still in their first sixteen updates) the probe update traces in one frame
    /// (ADR-7870, `specs/79` § 9.13); 0 (the default, and every tier's value) traces every young probe every
    /// frame. A start-up, a light cut or a far camera jump makes every probe young at once, and without a budget
    /// those frames trace the whole field: on High's grid sixteen frames of 62 ms on the Steam Deck. Past the
    /// budget a young probe waits for a later frame: the nearest to the eye first, then those in view, then the
    /// one that waited longest. A settled probe keeps its re-trace cadence and does not count. The field then
    /// takes [`Self::young_settle_frames`] frames to settle after a full restart, and the update freeze waits for
    /// that ([`DynamicGiBudget::young_settle_frames`](crate::DynamicGiBudget::young_settle_frames)). A tier does
    /// not write this field.
    pub young_budget: u32,
}

/// Default [`DdgiSettings::screen_max`]: the screen lookup region every tier reserved before it
/// was a setting.
pub const SCREEN_LOOKUP_MAX: [u32; 2] = [SCREEN_MAX.0, SCREEN_MAX.1];

/// Most seed weight [`DdgiSettings::seed_updates`] carries to the shaders (four bits of `ctrl.w`).
pub const MAX_SEED_UPDATES: u32 = 15;

impl Default for DdgiSettings {
    fn default() -> Self {
        Self {
            cascades: 4,
            dims: [20, 10, 20],
            spacing: 2.0,
            spacing_ratio: 3.0,
            update_divisor: 8,
            hysteresis: 0.97,
            normal_bias: 0.1,
            view_bias: 0.3,
            intensity: 1.0,
            max_terrain_ray: 3000.0,
            min_frontface: 0.12,
            visibility_limit: 2.0,
            max_instances_per_cascade: 64,
            min_proxy_radius: 1.0,
            max_proxy_voxel: 8.0,
            emitters: true,
            emitter_local_range: 0.0,
            bake_on_caller: false,
            sky_lod: 0.0,
            hf_cone: 0.08,
            hf_mean_surface: false,
            hf_shadow_cone: 0.02,
            screen_exact_debug: false,
            shading_off_debug: false,
            inactive_slowdown: 8,
            sdf_pool_bytes: SDF_POOL_BYTES,
            sdf_evict_frames: SDF_EVICT_FRAMES,
            specular: crate::ProbeSpecular::Lobe,
            seed_updates: 0,
            visibility_blend: false,
            screen_max: SCREEN_LOOKUP_MAX,
            recent_weight: 1,
            finest_headroom_m: FINEST_HEADROOM_M,
            front_weights: false,
            edge_visibility: false,
            young_budget: 0,
        }
    }
}

impl DdgiSettings {
    /// Most probe slots a volume may hold (the default is 16 000); larger requests are halved
    /// along their longest axis by [`Self::clamped`] so the probe buffers stay allocatable. That
    /// clamp also halves until both probe buffers fit one storage binding (the 128 MiB floor
    /// every WebGPU backend guarantees), so the true bound on a clamped layout is a byte
    /// budget, not only this count.
    pub const MAX_PROBES: u64 = 1 << 21;

    /// The largest whole-buffer storage binding the probe buffers are sized for: the 128 MiB
    /// floor WebGPU guarantees for `max_storage_buffer_binding_size` on every backend.
    pub(crate) const MAX_BINDING_BYTES: u32 = 128 << 20;

    /// These settings with `cascades` at most `MAX_CASCADES` and `dims` shrunk until
    /// [`Self::probe_count`] is at most [`Self::MAX_PROBES`] and both probe buffers fit one
    /// storage binding, and `sdf_pool_bytes` at most one storage binding (`MAX_BINDING_BYTES`,
    /// 128 MiB). A non-finite float is replaced with its default. `spacing` and
    /// `spacing_ratio` must be positive: zero or negative is replaced with the default, so a
    /// cascade corner cannot divide by zero or overflow.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        let defaults = Self::default();
        let finite = |value: f32, fallback: f32| {
            if value.is_finite() { value } else { fallback }
        };
        let positive = |value: f32, fallback: f32| {
            if value.is_finite() && value > 0.0 {
                value
            } else {
                fallback
            }
        };
        self.spacing = positive(self.spacing, defaults.spacing);
        self.spacing_ratio = positive(self.spacing_ratio, defaults.spacing_ratio);
        self.hysteresis = finite(self.hysteresis, defaults.hysteresis);
        self.normal_bias = finite(self.normal_bias, defaults.normal_bias);
        self.view_bias = finite(self.view_bias, defaults.view_bias);
        self.intensity = finite(self.intensity, defaults.intensity);
        self.max_terrain_ray = finite(self.max_terrain_ray, defaults.max_terrain_ray);
        self.min_frontface = finite(self.min_frontface, defaults.min_frontface);
        self.visibility_limit = finite(self.visibility_limit, defaults.visibility_limit);
        self.min_proxy_radius = finite(self.min_proxy_radius, defaults.min_proxy_radius);
        self.max_proxy_voxel = finite(self.max_proxy_voxel, defaults.max_proxy_voxel);
        self.emitter_local_range = finite(self.emitter_local_range, defaults.emitter_local_range);
        self.sky_lod = finite(self.sky_lod, defaults.sky_lod);
        self.hf_cone = finite(self.hf_cone, defaults.hf_cone);
        self.hf_shadow_cone = finite(self.hf_shadow_cone, defaults.hf_shadow_cone);
        self.cascades = self.cascades.min(MAX_CASCADES as u32);
        // `DdgiGpu` creates and binds the brick pool whole, like the probe buffers.
        self.sdf_pool_bytes = self.sdf_pool_bytes.min(u64::from(Self::MAX_BINDING_BYTES));
        self.dims = self.dims.map(|d| d.min(1024));
        while self.probe_count() > Self::MAX_PROBES || !self.probe_buffers_fit_one_binding() {
            let longest = (0..3).max_by_key(|&i| self.dims[i]).unwrap_or(0);
            self.dims[longest] = (self.dims[longest] / 2).max(1);
        }
        self
    }

    /// Whether the two probe buffers `DdgiGpu` binds whole (`ddgi data` and `ddgi rays`) fit one
    /// storage binding: the 128 MiB floor WebGPU guarantees for `max_storage_buffer_binding_size`
    /// on every backend. DX12 and non-NVIDIA Vulkan devices also cap `create_buffer` at
    /// `i32::MAX`, under which this floor sits, so respecting it keeps both the allocation and
    /// the whole-buffer bind under every shipping device's limits.
    fn probe_buffers_fit_one_binding(&self) -> bool {
        let floor = u64::from(Self::MAX_BINDING_BYTES);
        let (_, _, _, _, _, end) = DdgiState::data_words(self);
        end * 4 <= floor && self.probe_count() * u64::from(RAYS_PER_PROBE) * 16 <= floor
    }
    /// Probe slots in all cascades.
    #[must_use]
    pub fn probe_count(&self) -> u64 {
        u64::from(self.dims[0]) * u64::from(self.dims[1]) * u64::from(self.dims[2]) * u64::from(self.cascades)
    }

    /// Probe spacing of cascade `c`, metres.
    #[must_use]
    pub fn spacing_of(&self, c: u32) -> f32 {
        self.spacing * self.spacing_ratio.powi(c as i32)
    }

    /// Distance values the brick pool holds (`sdf_pool_bytes / 4`, capped so a brick offset
    /// fits in a `u32`).
    #[must_use]
    pub fn sdf_pool_values(&self) -> u64 {
        (self.sdf_pool_bytes / 4).min(u64::from(u32::MAX))
    }

    /// Whether this layout has a coarser cascade too wide to resolve a room, past
    /// [`ROOM_SPACING_M`]. Only then does the finest cascade hold the room around the eye and a
    /// ray hit keep to its own cascade (ADR-1949).
    pub(crate) fn needs_room_rule(&self) -> bool {
        let last = self.cascades.clamp(1, MAX_CASCADES as u32) - 1;
        last > 0 && self.spacing_of(last) > ROOM_SPACING_M
    }

    /// Frames of an unchanged traced scene after which the probe update stops: enough for every
    /// finest-cascade probe to take [`DDGI_FREEZE_UPDATES`] updates at this layout's update
    /// divisor, and never fewer than [`DDGI_FREEZE_MIN_FRAMES`] (ADR-5001).
    /// With a [`Self::young_budget`] it also waits until a field that restarted has taken its young updates
    /// ([`Self::young_settle_frames`]).
    pub(crate) fn freeze_frames(&self) -> u32 {
        DDGI_FREEZE_UPDATES
            .saturating_mul(packed_update_divisor(self))
            .max(DDGI_FREEZE_MIN_FRAMES)
            .max(self.young_settle_frames())
    }

    /// Frames after which every probe of a field that restarted (a start-up, a light cut) has taken its
    /// sixteen young updates, at most: 16 without a [`Self::young_budget`]. With a budget `B` a frame traces `B`
    /// young probes while at least `B` are young, so the `16 · probes` young updates take at most
    /// `⌊16 · probes / B⌋` such frames, and the fewer than `B` left then trace every frame for at most 16 more.
    #[must_use]
    pub fn young_settle_frames(&self) -> u32 {
        let young = u64::from(DDGI_YOUNG_UPDATES);
        if self.young_budget == 0 {
            return DDGI_YOUNG_UPDATES;
        }
        let frames = young * self.probe_count() / u64::from(self.young_budget) + young;
        u32::try_from(frames).unwrap_or(u32::MAX)
    }

    /// The screen lookup region in texels ([`Self::screen_max`], each side at least 1 and at most
    /// the default's [`SCREEN_LOOKUP_MAX`]).
    #[must_use]
    pub fn screen_region(&self) -> (u32, u32) {
        (
            self.screen_max[0].clamp(1, SCREEN_MAX.0),
            self.screen_max[1].clamp(1, SCREEN_MAX.1),
        )
    }

    /// Bytes of the screen lookup region ([`Self::screen_region`] × 8 words).
    #[must_use]
    pub fn screen_bytes(&self) -> u64 {
        let (w, h) = self.screen_region();
        u64::from(w) * u64::from(h) * SCREEN_WORDS * 4
    }

    /// The `ctrl.w` bits of the seeding and the visibility blend (ADR-2605): 0 for the defaults.
    fn seed_blend_bits(&self) -> u32 {
        (self.seed_updates.min(MAX_SEED_UPDATES) << CTRL_SEED_SHIFT)
            | ((self.recent_weight.clamp(1, 3) - 1) << CTRL_RECENT_SHIFT)
            | if self.visibility_blend {
                CTRL_VISIBILITY_BLEND
            } else {
                0
            }
    }
}

/// `DdgiUniform::ctrl.w` bit 23: [`DdgiSettings::front_weights`] (`ddgi_common.wgsl` `ddgi_front_weights_on`).
const CTRL_FRONT_WEIGHTS: u32 = 1 << 23;
/// `DdgiUniform::ctrl.w` bit 24: [`DdgiSettings::edge_visibility`] (`ddgi_common.wgsl`
/// `ddgi_edge_visibility_on`).
const CTRL_EDGE_VISIBILITY: u32 = 1 << 24;
/// `DdgiUniform::ctrl.w` bit 25: the image light's ground plane is on ([`DdgiLight::ground`],
/// `ddgi_trace.wgsl` `miss_radiance`). Its seven numbers ride components of the uniform nothing else
/// reads (`rot_*.w`, `sun.w`, `fill.w`, `emitters.zw`), so the buffer keeps its size.
const CTRL_GROUND_PLANE: u32 = 1 << 25;
/// `DdgiUniform::ctrl.w` bit: the shading keeps a point on the finest cascade where a coarser one
/// sees it much less (`ddgi_sample.wgsl` `ddgi_vis_take`, [`DdgiSettings::visibility_blend`]).
const CTRL_VISIBILITY_BLEND: u32 = 1 << 16;
/// `DdgiUniform::ctrl.w` bits 17..20: [`DdgiSettings::seed_updates`].
const CTRL_SEED_SHIFT: u32 = 17;
/// `DdgiUniform::ctrl.w` bits 21..22: [`DdgiSettings::recent_weight`] − 1.
const CTRL_RECENT_SHIFT: u32 = 21;

/// Cells from either end of the blend window (`ddgi_sample.wgsl` `ddgi_blend_inset`) to the first
/// point the cascade holds at full weight: the lookup's 0.75 cell margin plus its 1 cell ramp.
const BLEND_MARGIN_CELLS: f32 = 1.75;
/// The blend window's top end, in cells under `dims`: it stops a cell short of the last probe.
const BLEND_TOP_CELLS: f32 = 2.0;
/// The margin a blend window keeps before its one-cell ramp, cells (`ddgi_sample.wgsl` `ddgi_blend_weight`).
const PLAIN_MARGIN_CELLS: f32 = 0.75;
/// The same for the finest cascade of a layout that needs the room rule (`ddgi_sample.wgsl`
/// `DDGI_ROOM_MARGIN`, ADR-5909): a quarter cell. The volumes are placed as before ([`eye_lift_cells`]
/// keeps [`BLEND_MARGIN_CELLS`]); the lookup holds half a cell more of the finest cascade at each end
/// of each axis.
const ROOM_MARGIN_CELLS: f32 = 0.25;
/// Metres of full weight the finest cascade keeps above the eye by default (ADR-1949,
/// [`DdgiSettings::finest_headroom_m`]): a 3.5 m ceiling over a standing eye. Above that a lookup
/// reads a coarser cascade, whose spacing cannot resolve a thin roof.
pub const FINEST_HEADROOM_M: f32 = 2.0;
/// Metres of full weight the finest cascade keeps under the eye: the floor a standing eye looks
/// at. This comes before the headroom, because most of what a camera lights is under it and a
/// fine layout's window is short in metres (2.5 m at 1 m spacing and 8 layers).
const FINEST_FLOOR_M: f32 = 1.8;
/// The widest probe spacing, metres, a layout's coarsest cascade may have and still keep a closed
/// room dark without help: a sealed room read 0.04 % at 1 and 3 m cascades and 7.25 % at 2, 6, 18
/// and 54 m (`specs/79` § 9.1). Such a layout stays where it was, because the room rule pulls light
/// toward the finest probes, and 1 m probes under a fast camera are too young to hold it: they live
/// about 48 frames at 0.25 m a frame, and the walk of `tests/ddgi_temporal.rs` reads darker.
const ROOM_SPACING_M: f32 = 3.0;
/// `DdgiUniform::ctrl.w` bit: a ray hit reads the tracing probe's cascade or finer
/// (`ddgi_irradiance_traced`, [`DdgiSettings::needs_room_rule`]).
const CTRL_RAY_KEEPS_CASCADE: u32 = 1 << 14;

/// Cells the volume's centre sits under the eye (negative: over it) at `spacing` metres per cell.
/// Most of what a camera lights is under it, so a tall volume keeps 60 % of its height below the
/// eye, but every cascade reaches full weight at the eye's height and the finest one holds
/// [`FINEST_FLOOR_M`] under it and `headroom_m` (at most what the window has left) over it. At 8
/// probes per axis the plain
/// 60 % left the window's top under the eye: each cascade held 45 % of the eye's height and the
/// rest fell through to the 18 and 54 m cascades, which read a sealed room's upper walls from
/// outside it (ADR-1949, `specs/79` § 9.1).
fn eye_lift_cells(dims_y: u32, spacing: f32, c: u32, headroom_m: f32) -> f32 {
    let half = (dims_y / 2) as f32;
    let top = dims_y as f32 - BLEND_TOP_CELLS - BLEND_MARGIN_CELLS - half;
    let lift = (0.1 * dims_y as f32).min(top).max(0.0);
    if c == 0 {
        let headroom_m = if headroom_m.is_finite() && headroom_m >= 0.0 {
            headroom_m
        } else {
            FINEST_HEADROOM_M
        };
        let floor = (BLEND_MARGIN_CELLS + FINEST_FLOOR_M / spacing - half).min(top);
        // A short volume keeps the eye inside the window's bottom end.
        lift.min(top - headroom_m / spacing)
            .max(floor)
            .max((BLEND_MARGIN_CELLS - half).min(0.0))
    } else {
        lift
    }
}

/// Cells the centre of cascade `c` sits under the eye in this layout: [`eye_lift_cells`], or the
/// plain 60 % of the height under the eye where no cascade needs the room rule.
fn cascade_lift_cells(settings: &DdgiSettings, c: u32) -> f32 {
    let dims_y = settings.dims[1];
    if settings.needs_room_rule() {
        eye_lift_cells(dims_y, settings.spacing_of(c), c, settings.finest_headroom_m)
    } else {
        0.1 * dims_y as f32
    }
}

/// How far around the eye cascade 0 holds at full weight, metres: (above, below). What the
/// finest probes resolve; a room taller than this reads the next cascade near its ceiling or
/// floor.
pub(crate) fn finest_window_m(settings: &DdgiSettings) -> (f32, f32) {
    let dims_y = settings.dims[1];
    let eye = (dims_y / 2) as f32 + cascade_lift_cells(settings, 0);
    // The finest window's margin and its one-cell ramp (`ddgi_blend_weight`).
    let margin = finest_margin_cells(settings) + 1.0;
    let above = dims_y as f32 - BLEND_TOP_CELLS - margin - eye;
    let below = eye - margin;
    (
        above.max(0.0) * settings.spacing,
        below.max(0.0) * settings.spacing,
    )
}

/// The margin of the finest cascade's blend window before its ramp, cells (`ddgi_sample.wgsl`
/// `ddgi_blend_weight`): a quarter cell on a layout that needs the room rule (ADR-5909), three quarters
/// otherwise.
fn finest_margin_cells(settings: &DdgiSettings) -> f32 {
    if settings.needs_room_rule() {
        ROOM_MARGIN_CELLS
    } else {
        PLAIN_MARGIN_CELLS
    }
}

/// World cell of the lowest-corner probe of a cascade around `eye`. The volume is centred on
/// the eye in x and z, and its centre sits `lift` cells under the eye (`cascade_lift_cells`).
///
/// A non-finite eye axis is treated as 0. A finite eye past the `i32` cell grid saturates,
/// so this does not overflow.
#[must_use]
pub fn cascade_base(eye: [f32; 3], spacing: f32, dims: [u32; 3], lift: f32) -> [i32; 3] {
    let centre = cascade_centre_cells(eye, spacing, lift);
    std::array::from_fn(|a| {
        let snapped = centre[a].floor() as i32;
        snapped.saturating_sub((dims[a] / 2) as i32)
    })
}

/// Highest probe cell on one axis (`base + dim - 1`).
///
/// Saturating, so a non-finite spacing (the base becomes `i32::MAX` when `floor` of
/// `+inf` saturates) cannot overflow `prepare`.
fn cascade_hi_cell(base: i32, dim: u32) -> i32 {
    let span = dim.saturating_sub(1).min(i32::MAX as u32) as i32;
    base.saturating_add(span)
}

/// The lowest corner of a cascade around `eye` before it snaps to whole cells, in cells:
/// [`cascade_base`] plus the eye's position inside its cell, so every component lies in
/// `[base, base + 1]`. The shading lookup blends one cascade into the next over a window
/// measured from this corner (`ddgi_sample.wgsl` `ddgi_blend_inset`). The window moves with
/// the eye, while the probes stay on whole cells, so a scroll step does not change the blend.
#[must_use]
pub fn cascade_origin(eye: [f32; 3], spacing: f32, dims: [u32; 3], lift: f32) -> [f32; 3] {
    let centre = cascade_centre_cells(eye, spacing, lift);
    let base = cascade_base(eye, spacing, dims, lift);
    std::array::from_fn(|a| {
        let frac = centre[a] - centre[a].floor();
        let frac = if frac.is_finite() { frac } else { 0.0 };
        base[a] as f32 + frac
    })
}

/// The point a cascade centres on, in cells of `spacing` (see `cascade_base`).
///
/// A non-finite eye component is 0, and a non-finite lift is 0, so a bad camera does not
/// put NaN into the probe grid.
fn cascade_centre_cells(eye: [f32; 3], spacing: f32, lift: f32) -> [f32; 3] {
    std::array::from_fn(|a| {
        let eye_a = if eye[a].is_finite() { eye[a] } else { 0.0 };
        let centre = if a == 1 {
            let lift = if lift.is_finite() { lift } else { 0.0 };
            eye_a - lift * spacing
        } else {
            eye_a
        };
        centre / spacing
    })
}

/// Uniform random rotation (Shoemake 1992) from three numbers in `[0, 1)`: the rows.
#[must_use]
pub fn random_rotation(u: [f32; 3]) -> [[f32; 3]; 3] {
    let tau = std::f32::consts::TAU;
    let (s1, s2) = ((1.0 - u[0]).sqrt(), u[0].sqrt());
    let (t1, t2) = (tau * u[1], tau * u[2]);
    let (x, y, z, w) = (s1 * t1.sin(), s1 * t1.cos(), s2 * t2.sin(), s2 * t2.cos());
    [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - w * z),
            2.0 * (x * z + w * y),
        ],
        [
            2.0 * (x * y + w * z),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - w * x),
        ],
        [
            2.0 * (x * z - w * y),
            2.0 * (y * z + w * x),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ]
}

/// The trace cadence words of the uniform: `ctrl.z` (frames between a settled probe's
/// re-traces) and the inactive-probe slowdown shift (`ctrl.w` bits 8..12). `ddgi_trace.wgsl`
/// shifts `ctrl.z` left by the cascade (up to 3) and by the slowdown shift.
/// `ctrl.z` is [`packed_update_divisor`] (clamped to [`DDGI_MAX_UPDATE_DIVISOR`]) so the widest
/// product (`<< 3 << 15`) stays below 2^32: a wrapped divisor would change the cadence, and 0
/// would make the shader's `% divisor` indeterminate (audit-v035-wgsl 4, audit v034 WGSL L1).
fn update_cadence(s: &DdgiSettings) -> (u32, u32) {
    (
        packed_update_divisor(s),
        s.inactive_slowdown.max(1).ilog2().min(15),
    )
}

fn hash01(mut v: u32) -> f32 {
    v ^= v >> 16;
    v = v.wrapping_mul(0x7feb_352d);
    v ^= v >> 15;
    v = v.wrapping_mul(0x846c_a68b);
    v ^= v >> 16;
    (v >> 8) as f32 / (1u32 << 24) as f32
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct CascadeGpu {
    origin: [f32; 4],
    base: [i32; 4],
}

/// `DdgiUniform` in `ddgi_common.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct DdgiUniformGpu {
    cascades: [CascadeGpu; MAX_CASCADES],
    dims: [u32; 4],
    params: [f32; 4],
    ctrl: [u32; 4],
    rot: [[f32; 4]; 3],
    to_sun: [f32; 4],
    sun: [f32; 4],
    sky: [f32; 4],
    flat_sky: [f32; 4],
    inst_offset: [u32; 4],
    inst_count: [u32; 4],
    words: [u32; 4],
    screen: [u32; 4],
    emitters: [u32; 4],
    to_fill: [f32; 4],
    fill: [f32; 4],
    /// The frame's extra directional lights (ADR-7254): per slot the direction toward the light
    /// (`w` = 1 when on), then illuminance × colour (lux). Unshadowed, as the forward shades them.
    extra_dir: [[f32; 4]; 2 * crate::MAX_EXTRA_DIRECTIONAL],
}

impl DdgiUniformGpu {
    /// A uniform with only the shading flags (`ctrl.w`) and the screen lookup set: what the
    /// forward and terrain passes read when another source (the baked volume) fills the screen
    /// lookup.
    pub(crate) fn screen_only(screen: [u32; 4], flags: u32) -> Self {
        Self {
            ctrl: [0, 0, 0, flags],
            params: [0.0, 0.0, 0.0, 1.0],
            screen,
            ..Self::default()
        }
    }

    pub(crate) fn cascade_base(&self, c: usize) -> [i32; 4] {
        self.cascades[c].base
    }

    pub(crate) fn dims(&self) -> [u32; 4] {
        self.dims
    }

    pub(crate) fn words(&self) -> [u32; 4] {
        self.words
    }

    pub(crate) fn inst_count(&self) -> [u32; 4] {
        self.inst_count
    }
}

/// `DdgiEmitter` in `ddgi_blend.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct EmitterGpu {
    /// World centre, radius (m).
    pub(crate) pos_radius: [f32; 4],
    /// Radiance × area (rgb, cd): the cluster's flux is π times it; w = the short-range
    /// emissive pass's range for this mesh (m, 0: none; then the global
    /// `DdgiSettings::emitter_local_range` applies).
    pub(crate) power: [f32; 4],
    /// Mean outward normal (xyz) and flatness (w: |Σ area · normal| / Σ area, 1 for a flat
    /// one-sided patch such as a strip on a hull, 0 for a closed blob).
    pub(crate) normal_flat: [f32; 4],
}

/// One emitter ranked for the frame's cap: strength at the eye, then mesh order, then cell.
struct RankedEmitter {
    strength: f32,
    mesh_index: usize,
    cell: [u32; 3],
    emitter: EmitterGpu,
}

/// World-space cluster geometry that does not depend on the eye. Strength is
/// `strength_num / (distance² + radius²)` and is recomputed when the eye moves.
#[derive(Clone, Copy)]
struct EmitterWorldRecord {
    cell: [u32; 3],
    centre: Vec3,
    radius: f32,
    strength_num: f32,
    emitter: EmitterGpu,
}

/// Cached world emitters of one mesh. A matrix, radiance or local-range change misses.
struct EmitterWorldMemo {
    model_bits: [u32; 16],
    radiance_bits: [u32; 3],
    local_range_bits: u32,
    records: Vec<EmitterWorldRecord>,
}

/// One cluster of a mesh's triangles, object space.
#[derive(Clone, Copy, Debug)]
struct EmitterCluster {
    /// Grid cell the cluster's triangles fell into (`emitter_clusters`' key): the
    /// deterministic tie-break for equal strengths at the emitter cap.
    cell: [u32; 3],
    centroid: Vec3,
    area: f32,
    half_diagonal: f32,
    /// Σ area · outward normal.
    normal_sum: Vec3,
}

/// A mesh's triangles grouped on a grid of at most [`EMITTER_CELLS`] cells along its longest
/// axis: area-weighted centroid, area and cell half diagonal per occupied cell.
fn emitter_clusters(mesh: &FrameMesh<'_>) -> Vec<EmitterCluster> {
    let p = |i: u32| {
        mesh.vertices
            .get(i as usize)
            .map(|v| Vec3::from_array(v.position))
    };
    let vn = |i: u32| {
        mesh.vertices
            .get(i as usize)
            .map_or(Vec3::ZERO, |v| Vec3::from_array(v.normal))
    };
    let mut lo = Vec3::splat(f32::MAX);
    let mut hi = Vec3::splat(f32::MIN);
    for v in mesh.vertices {
        lo = lo.min(Vec3::from_array(v.position));
        hi = hi.max(Vec3::from_array(v.position));
    }
    let extent = (hi - lo).max(Vec3::splat(1e-4));
    let cell = (extent.max_element() / EMITTER_CELLS).max(1e-3);
    let dims = (extent / cell).ceil().max(Vec3::ONE);
    let mut cells: HashMap<[u32; 3], (Vec3, f32, Vec3)> = HashMap::new();
    for tri in mesh.indices.as_chunks::<3>().0 {
        let (Some(a), Some(b), Some(c)) = (p(tri[0]), p(tri[1]), p(tri[2])) else {
            continue;
        };
        let cross = (b - a).cross(c - a);
        let area = 0.5 * cross.length();
        if area <= 0.0 {
            continue;
        }
        // Outward as the renderer shades it: the winding, flipped where the vertex normals
        // disagree (as the bricks are signed).
        let mut n = cross / (2.0 * area);
        if n.dot(vn(tri[0]) + vn(tri[1]) + vn(tri[2])) < 0.0 {
            n = -n;
        }
        let centre = (a + b + c) / 3.0;
        let g = ((centre - lo) / cell).floor().clamp(Vec3::ZERO, dims - Vec3::ONE);
        let entry =
            cells
                .entry([g.x as u32, g.y as u32, g.z as u32])
                .or_insert((Vec3::ZERO, 0.0, Vec3::ZERO));
        entry.0 += centre * area;
        entry.1 += area;
        entry.2 += n * area;
    }
    let half_diagonal = 0.5 * Vec3::splat(cell).min(extent).length();
    // Keyed by cell and sorted by it: `HashMap` iteration order is per-process, and the list
    // order decides equal-strength ties at the emitter cap, so the cluster order must not
    // come out of the map (audit fix-render4 #2).
    let mut out: Vec<EmitterCluster> = cells
        .into_iter()
        .filter(|(_, (_, area, _))| *area > 0.0)
        .map(|(cell, (sum, area, normal_sum))| EmitterCluster {
            cell,
            centroid: sum / area,
            area,
            half_diagonal,
            normal_sum,
        })
        .collect();
    out.sort_by_key(|cluster| cluster.cell);
    out
}

/// The probe volumes read back from the GPU (tests and debugging).
#[derive(Clone, Debug)]
pub struct DdgiReadback {
    /// Probe data words (`ddgi_common.wgsl` layout).
    pub data: Vec<u32>,
    /// Last rays: per probe slot, 64 × (radiance rgb, hit distance; < 0 a
    /// back face, 1e9 a miss).
    pub rays: Vec<[f32; 4]>,
    /// Lowest-corner world cell per cascade.
    pub bases: [[i32; 3]; MAX_CASCADES],
    /// Probes per axis.
    pub dims: [u32; 3],
    /// Cascades in use.
    pub cascades: u32,
    /// First irradiance and first distance word.
    pub words: [u32; 2],
    /// Brick instances traced this frame.
    pub instances: u32,
    /// Instances per cascade list.
    pub lists: [u32; MAX_CASCADES],
    /// Screen lookup (half resolution unless measured at another): first word, width, height
    /// (0 × 0 when it is off).
    pub screen: [u32; 3],
}

impl DdgiReadback {
    /// The last frame's screen lookup: (width, height, texels). `None` when
    /// the lookup did not run.
    #[must_use]
    pub fn screen_irradiance(&self) -> Option<(u32, u32, Vec<DdgiScreenTexel>)> {
        let [base, w, h] = self.screen;
        if w == 0 || h == 0 {
            return None;
        }
        let words = SCREEN_WORDS as usize;
        let start = base as usize;
        let end = start + (w * h) as usize * words;
        let region = self.data.get(start..end)?;
        Some((
            w,
            h,
            region
                .chunks_exact(words)
                .map(|t| DdgiScreenTexel {
                    e: [0, 1, 2, 3, 4, 5].map(|k| unpack_rgb9e5(t[k])),
                    coverage: f32::from_bits(t[7]),
                })
                .collect(),
        ))
    }

    /// Slot of world cell `cell` in cascade `c`.
    #[must_use]
    pub fn slot(&self, c: u32, cell: [i32; 3]) -> u32 {
        let d = self.dims.map(|v| v as i32);
        let s: [i32; 3] = std::array::from_fn(|a| cell[a].rem_euclid(d[a]));
        c * (self.dims[0] * self.dims[1] * self.dims[2]) + (s[0] + d[0] * (s[1] + d[1] * s[2])) as u32
    }

    /// (offset xyz + active, world cell xyz + updates) of a slot.
    #[must_use]
    pub fn probe(&self, slot: u32) -> ([f32; 4], [i32; 4]) {
        let b = slot as usize * PROBE_WORDS as usize;
        (
            std::array::from_fn(|i| f32::from_bits(self.data[b + i])),
            std::array::from_fn(|i| self.data[b + 4 + i] as i32),
        )
    }

    /// Irradiance / π of a slot toward octahedral texel (x, y) of the 6×6 interior. The 16 texels
    /// next to a coordinate axis hold that axis's lobe on every layout (`ddgi_irr_texel_dir` in
    /// `ddgi_common.wgsl`, ADR-5374, ADR-5506): those with `x + y` even at the
    /// probe, those with `x + y` odd at the surface that lies up to 1.05 spacings behind the probe
    /// along the axis, when there is one (ADR-5457, `ddgi_axis_texels`); (2, 2) is +z at the probe.
    #[must_use]
    pub fn irradiance(&self, slot: u32, x: u32, y: u32) -> [f32; 3] {
        let w = self.words[0] as usize
            + slot as usize * IRR_WORDS as usize
            + (((y + 1) * 8 + x + 1) * 3) as usize;
        std::array::from_fn(|i| f32::from_bits(self.data[w + i]))
    }

    /// Irradiance / π of a slot at the crease point of face `face` (+x −x +y −y +z −z, the surface
    /// behind the probe along that direction) toward the face's side `side` (0..4: the two axes
    /// across the face, each toward and away, `ddgi_crease_side` in `ddgi_common.wgsl`): the border
    /// texel `ddgi_crease_texel` names (ADR-5552). Where the probe holds no crease there (no flat
    /// surface within reach behind it or on that side) it is the face's own light, the value
    /// [`Self::irradiance`] returns for the face's texel at the surface.
    #[must_use]
    pub fn crease_irradiance(&self, slot: u32, face: u32, side: u32) -> [f32; 3] {
        let (x, y) = crease_texel(face, side);
        let w = self.words[0] as usize + slot as usize * IRR_WORDS as usize + ((y * 8 + x) * 3) as usize;
        std::array::from_fn(|i| f32::from_bits(self.data[w + i]))
    }
}

/// Tile texel (border included, 8 × 8) of the crease of face `face` (0..6) toward its side `side`
/// (0..4): `ddgi_crease_texel` in `ddgi_common.wgsl` (ADR-5552). The 24 creases run along the tile's
/// border ring; the corner (0, 0) holds the surface word.
fn crease_texel(face: u32, side: u32) -> (u32, u32) {
    let i = face.min(5) * 4 + side.min(3);
    match i {
        0..=6 => (i + 1, 0),
        7..=14 => (i - 7, 7),
        15..=20 => (0, i - 14),
        _ => (7, i - 20),
    }
}

/// `Heightfield` in `ddgi_trace.wgsl` / `ddgi_maxmip.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct HeightfieldGpu {
    pub(crate) map: [f32; 4],
    pub(crate) size: [u32; 4],
    pub(crate) albedo: [f32; 4],
    /// x = the sun shadow rays' cone footprint, negative for the mean-height coarse surface;
    /// yzw = `HeightfieldInfo::origin_offset`.
    pub(crate) shadow: [f32; 4],
}

/// `SdfInstance` in `ddgi_trace.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct SdfInstanceGpu {
    w2l: [[f32; 4]; 3],
    aabb_min: [f32; 4],
    aabb_max: [f32; 4],
    brick_origin: [f32; 4],
    brick_voxel: [f32; 4],
    brick: [u32; 4],
    /// rgb = the mean albedo, w = the material's perceptual roughness ([`hit_roughness`]).
    albedo: [f32; 4],
    /// rgb = the mean emitted radiance, w = the material's metallic factor ([`hit_metallic`]).
    emissive: [f32; 4],
}

/// Least perceptual roughness an instance record carries: 0 in the record means "no material model"
/// (`ddgi_trace.wgsl` `Hit::rough`, a terrain hit).
const HIT_ROUGHNESS_MIN: f32 = 0.05;

/// The perceptual roughness a probe ray's hit on `material` reflects with (ADR-5506): the material's
/// roughness factor, at least [`HIT_ROUGHNESS_MIN`]. A roughness map is not read; the factor stands
/// for the surface.
fn hit_roughness(material: &crate::LabMaterial) -> f32 {
    if material.roughness.is_finite() {
        material.roughness.clamp(HIT_ROUGHNESS_MIN, 1.0)
    } else {
        1.0
    }
}

/// The metallic factor a probe ray's hit on `material` reflects with (ADR-5506): that share of the
/// hit keeps the plain albedo reflection, the rest reflects as the forward pass's dielectric.
fn hit_metallic(material: &crate::LabMaterial) -> f32 {
    if material.metallic.is_finite() {
        material.metallic.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// A baked brick resident in the GPU pool.
#[derive(Clone, Copy, Debug)]
struct BrickSlot {
    /// First distance value in the pool.
    offset: u32,
    /// Distance values (the GPU span).
    len: u32,
    origin: [f32; 3],
    voxel: [f32; 3],
    dims: [u32; 3],
    /// [`DdgiState::frame`] this mesh was last drawn.
    last_drawn: u32,
}

/// A free span in the brick pool (value index, length).
#[derive(Clone, Copy, Debug)]
struct FreeSpan {
    offset: u64,
    len: u64,
}

/// A mesh that did not fit, remembered until [`DdgiState::pool_gen`] moves.
///
/// The generation moves when a brick is freed. A budget change drops the whole
/// [`DdgiState`] (`DdgiGpu::matches` includes the value count), so this memo
/// dies with the old volume. While it matches, the frame does not walk the
/// mesh or build bake inputs.
#[derive(Clone, Copy, Debug)]
struct UnfitMemo {
    verts: usize,
    indices: usize,
    /// [`DdgiState`] pool generation this refusal was recorded at.
    generation: u64,
}

/// [`brick_value_count`] for one `(mesh key, vertex count, index count)`.
///
/// `None` means the mesh cannot be voxelised. Dropped after
/// [`EXTENT_IDLE_FRAMES`] unseen updates, the same idle rule as the
/// object-space bounds cache.
#[derive(Clone, Copy, Debug)]
struct CachedBrickCount {
    count: Option<u64>,
    /// Last frame (`DdgiState::frame`) that drew this mesh while the count was consulted.
    seen: u32,
}

/// Light and sky the probes are traced under this frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DdgiLight {
    /// Direction toward the sun, `None` when it is down or off.
    pub to_sun: Option<[f32; 3]>,
    /// Sun illuminance × colour (lux).
    pub sun: [f32; 3],
    /// Scale from the sky map's values to radiance (0 = no map: `flat_sky`).
    pub sky_scale: f32,
    /// Sky map lod the misses read.
    pub sky_lod: f32,
    /// Radiance of a uniform sky when no map is bound.
    pub flat_sky: [f32; 3],
    /// The sky body's planet-shine: direction toward it and illuminance × tint (lux), as the
    /// forward pass's fill (`OutdoorState::fill_light`); `None` without one.
    pub fill: Option<[[f32; 3]; 2]>,
    /// The image light's ground plane (`env_ground.rs`, ADR-2000): the frame's `env_ground`
    /// uniform (height, radius or 0 for unbounded, centre x, centre z) and the plane's radiance;
    /// `None` without one. A probe ray that misses every surface and lands on the plane reads the
    /// plane instead of the image's lower half (ADR-5716), as the forward pass's own sky term does.
    pub ground: Option<([f32; 4], [f32; 3])>,
    /// The frame's extra directional lights besides the sun (ADR-7254): `[toward the light,
    /// illuminance × colour]`, at most [`crate::MAX_EXTRA_DIRECTIONAL`] (`None` slots are off).
    pub extra: [Option<[[f32; 3]; 2]>; crate::MAX_EXTRA_DIRECTIONAL],
}

impl DdgiLight {
    /// The extra lights of a frame's packed rows ([`crate::SharedFrameInfo::extra_directional`]:
    /// travel direction with `w` = 1 when on, then colour × lux). A light pointing upward (from
    /// below the horizon) or with no light is off.
    pub(crate) fn extra_from_rows(
        rows: &[[f32; 4]; 2 * crate::MAX_EXTRA_DIRECTIONAL],
    ) -> [Option<[[f32; 3]; 2]>; crate::MAX_EXTRA_DIRECTIONAL] {
        std::array::from_fn(|i| {
            let [x, y, z, on] = rows[2 * i];
            let [r, g, b, _] = rows[2 * i + 1];
            let len = (x * x + y * y + z * z).sqrt();
            (on > 0.5 && len > 1e-6 && y < 0.0 && r.max(g).max(b) > 0.0)
                .then(|| [[-x / len, -y / len, -z / len], [r, g, b]])
        })
    }
}

/// The RGBA8 texels of one of a material's maps as the probes read them: the standard slot's
/// (`standard`), or graph slot `slot` (0 base colour, 3 emissive) of a foliage material whose maps
/// [`crate::FoliageSway::adopt`] moved into its [`crate::FOLIAGE_SURFACE`] graph (ADR-5716).
///
/// Until that ADR the probes read the standard slots alone. A swaying plant placed in the frame's
/// mesh list has none, so its hits reflected its base colour factor (white for most plants), while
/// the same plant in an instance set, which draws under its plain twin
/// (`VisBufferPass::set_instance_set`), reflected its map's mean: the two ways of drawing one plant
/// gave the probes different scenes, against ADR-5457 (the visibility buffer's wind pair, once the
/// probes saw a flat card at all).
fn map_texels<'a>(
    standard: Option<&'a crate::LabTexture>,
    material: &'a crate::LabMaterial,
    slot: usize,
) -> Option<&'a crate::LabTexture> {
    standard.or_else(|| {
        let graph = material
            .surface_graph
            .as_deref()
            .filter(|graph| &*graph.wgsl == crate::FOLIAGE_SURFACE)?;
        let texture = graph.textures.get(slot)?.as_ref()?;
        (!texture.image.rgba.is_empty()).then_some(&texture.image)
    })
}

/// Most instances a set may have and still enter the probes' scene (ADR-5457). The instances the
/// probes list are picked by a pass over the whole set whenever the set or the cascades' cells
/// change (`DdgiState::set_pick`), 16 384 instances in under a millisecond; a larger set (a
/// scattered forest) needs a spatial index for that pick and stays out of the probes' scene until
/// it has one, as every set did before.
pub(crate) const PROBE_SET_MAX_INSTANCES: usize = 16_384;

/// A persistent visibility-buffer instance set as the probes trace it (ADR-5457): many instances
/// of one rigid mesh that is not in the frame's mesh list (`VisBufferPass::probe_sets`). Each
/// instance enters the probes' brick instance list as a placed [`FrameMesh`] of the same mesh,
/// material and model would, so both ways of drawing a mesh give the probes the same scene.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProbeSet<'a> {
    /// The set's key (`FrameRenderer::set_instance_set`).
    pub set_key: u64,
    /// A value that changes whenever any set changes (`VisBufferPass::sets_generation`): the
    /// probes keep each set's instance pick while it stays.
    pub generation: u64,
    /// The mesh's key (bricks are per mesh key, shared with a placed mesh of the same key).
    pub mesh_key: u64,
    /// The material's key (the mean albedo cache).
    pub material_key: u64,
    /// The material the set draws with.
    pub material: &'a crate::LabMaterial,
    /// Vertex positions, object space.
    pub positions: &'a [[f32; 3]],
    /// Vertex normals (empty: the brick is signed by the winding alone).
    pub normals: &'a [[f32; 3]],
    /// Triangle indices into `positions`.
    pub indices: &'a [u32],
    /// One model matrix per instance.
    pub models: &'a [[[f32; 4]; 4]],
    /// Shadow policy of every instance (unless `shadows` is given).
    pub shadow: ShadowCast,
    /// One shadow policy per instance (the length of `models`), or empty.
    pub shadows: &'a [ShadowCast],
    /// The smallest and largest instance scale (each instance's longest model axis).
    pub scale_range: (f32, f32),
}

/// The geometry the probes trace this frame: the frame's meshes and the persistent instance sets
/// that draw beside them (ADR-5457).
#[derive(Clone, Copy)]
pub(crate) struct ProbeGeometry<'a, 'm> {
    /// The frame's mesh list.
    pub meshes: &'a [FrameMesh<'m>],
    /// The instance sets, in set-key order.
    pub sets: &'a [ProbeSet<'a>],
}

impl ProbeSet<'_> {
    /// The material and shape half of the proxy test, as [`proxy_eligible`] for a placed mesh.
    fn eligible(&self) -> bool {
        !(self.mesh_key == 0
            || self.material.unlit
            || crate::hair::effective_alpha_mode(self.material) == crate::lab_frame::ALPHA_BLEND
            || self.indices.len() < 3
            || self.models.is_empty()
            || self.models.len() > PROBE_SET_MAX_INSTANCES
            || (self.shadows.len() != self.models.len() && self.shadow == ShadowCast::Off))
    }

    /// Shadow policy of instance `i`.
    fn shadow_of(&self, i: usize) -> ShadowCast {
        if self.shadows.len() == self.models.len() {
            self.shadows[i]
        } else {
            self.shadow
        }
    }
}

/// Whether the light jumped between two frames (a time-of-day set, a teleport between
/// levels, a light switched): the sun turned more than ~3°, came up or went down, or its
/// illuminance or the sky's scale changed by more than a quarter. The probes then re-converge
/// as new ones (eight fresh updates) instead of fading over the hysteresis (~1 s); a sun that
/// moves smoothly with the clock never trips it.
fn light_cut(a: &DdgiLight, b: &DdgiLight) -> bool {
    let lum = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let ratio_off = |x: f32, y: f32| {
        let (lo, hi) = (x.min(y), x.max(y));
        hi > 1e-6 && lo < hi * 0.75
    };
    let turned = match (a.to_sun, b.to_sun) {
        (Some(p), Some(q)) => p[0] * q[0] + p[1] * q[1] + p[2] * q[2] < 0.9986,
        (None, None) => false,
        _ => true,
    };
    let fill = |l: &DdgiLight| l.fill.map_or(0.0, |[_, c]| lum(c));
    // The image light's ground plane set, taken away, moved or changed by more than a quarter: the
    // lower half of what every probe sees (ADR-5716).
    let ground = match (a.ground, b.ground) {
        (Some((p, l)), Some((q, m))) => {
            ratio_off(lum(l), lum(m)) || p.iter().zip(&q).any(|(x, y)| (x - y).abs() > 0.01)
        }
        (None, None) => false,
        _ => true,
    };
    // The extra lights: one switched on or off, or turned like the sun, is a jump too.
    let extra_cut = a.extra.iter().zip(&b.extra).any(|pair| match pair {
        (Some([p, c]), Some([q, e])) => {
            p[0] * q[0] + p[1] * q[1] + p[2] * q[2] < 0.9986 || ratio_off(lum(*c), lum(*e))
        }
        (None, None) => false,
        _ => true,
    });
    turned
        || ground
        || extra_cut
        || ratio_off(lum(a.sun), lum(b.sun))
        || ratio_off(fill(a), fill(b))
        || ratio_off(a.sky_scale, b.sky_scale)
        || ratio_off(lum(a.flat_sky), lum(b.flat_sky))
}

/// The terrain the probes trace (from [`crate::SharedPassDraw::gi_heightfield`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeightfieldInfo {
    /// Samples per side (square map, centred on the world origin).
    pub samples: u32,
    /// Half the map width, metres.
    pub half_width: f32,
    /// Map width, metres.
    pub world_width: f32,
    /// World height = texel × `height_scale` + `height_offset`.
    pub height_scale: f32,
    /// See `height_scale`.
    pub height_offset: f32,
    /// Ground albedo the probe rays see (where no albedo map is bound).
    pub albedo: [f32; 3],
    /// Identity of the albedo map ([`crate::SharedPassDraw::gi_albedo_map`]); 0 = none.
    pub albedo_generation: u64,
    /// Changes whenever the heights change.
    pub generation: u64,
    /// The local origin's position in the heightfield's space (`SharedFrameInfo::origin_offset`,
    /// spec 78 § 12.2): zero until a floating-origin rebase. Rays are local; the trace adds this.
    pub origin_offset: [f32; 3],
    /// The sample rectangles the last `generation` step changed (ADR-7659): the probes'
    /// max-height pyramid re-reduces only the texels they reach. The default (no rectangles)
    /// rebuilds the whole pyramid on a new generation, as before.
    pub edits: HeightEdits,
}

/// Most rectangles [`HeightEdits`] holds: one 256-byte uniform slot per pyramid level (wgpu's
/// largest uniform offset alignment) holds 16 `vec4<u32>` (ADR-7659). A 17th rectangle merges
/// into the last one (their bounding rectangle), which still re-reduces less than the whole map.
pub const HEIGHT_EDIT_RECTS: usize = 16;

/// The height samples one generation step of a [`HeightfieldInfo`] changed (ADR-7659).
///
/// The rectangles list every sample whose height differs between `from_generation` and the
/// info's `generation`. A consumer whose copy was built at any other generation, or that sees
/// no rectangle, rebuilds from the whole map. The producer replaces the record on every
/// generation step, so nothing needs clearing: a later whole-map change writes the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct HeightEdits {
    /// The generation the rectangles lead from (to `HeightfieldInfo::generation`).
    pub from_generation: u64,
    /// Rectangles in use (0 = no regional record: whole-map change).
    pub count: u32,
    /// Sample rectangles `[col_start, row_start, col_end, row_end]`, ends exclusive.
    pub rects: [[u32; 4]; HEIGHT_EDIT_RECTS],
}

impl HeightEdits {
    /// An empty record leading from `from_generation`.
    #[must_use]
    pub fn since(from_generation: u64) -> Self {
        Self {
            from_generation,
            ..Self::default()
        }
    }

    /// Adds the changed samples `[col_start, row_start, col_end, row_end]` (ends exclusive).
    /// An empty rectangle is ignored; past [`HEIGHT_EDIT_RECTS`] the rectangle merges into the
    /// last slot.
    pub fn push(&mut self, rect: [u32; 4]) {
        if rect[0] >= rect[2] || rect[1] >= rect[3] {
            return;
        }
        let n = self.count as usize;
        if n < HEIGHT_EDIT_RECTS {
            self.rects[n] = rect;
            self.count += 1;
        } else {
            let last = &mut self.rects[HEIGHT_EDIT_RECTS - 1];
            *last = [
                last[0].min(rect[0]),
                last[1].min(rect[1]),
                last[2].max(rect[2]),
                last[3].max(rect[3]),
            ];
        }
    }

    /// The rectangles in use.
    #[must_use]
    pub fn rects(&self) -> &[[u32; 4]] {
        &self.rects[..(self.count as usize).min(HEIGHT_EDIT_RECTS)]
    }
}

/// CPU state of the probe volumes: bricks, instances and the uniform.
pub(crate) struct DdgiState {
    pub(crate) settings: DdgiSettings,
    frame: u32,
    bricks: HashMap<u64, BrickSlot>,
    pending: HashMap<u64, regia_core::tasks::Task<Option<SdfBrick>>>,
    failed: HashSet<u64>,
    albedo: HashMap<u64, [f32; 3]>,
    /// Mean linear emissive map per material key (white without a map).
    emissive_map_mean: HashMap<u64, [f32; 3]>,
    /// Emitter clusters per mesh key (object space).
    clusters: HashMap<u64, Vec<EmitterCluster>>,
    /// Object-space bounds per `(mesh key, vertex count)` for [`proxy_candidate`], so a frame
    /// does not walk every vertex of every mesh (v0.1.4: 20–50 ms of `ddgi prepare` in a
    /// skirmish). Entries unseen for [`EXTENT_IDLE_FRAMES`] frames are dropped.
    extents: HashMap<(u64, usize), MeshExtent>,
    /// This frame's emitter list.
    pub(crate) emitters: Vec<EmitterGpu>,
    /// Bricks that did not fit. Retried while their mesh is still drawn; not a permanent failure.
    parked: HashMap<u64, SdfBrick>,
    /// Mesh keys the host opted out of GI proxies (`MeshRenderer::gi_proxy` / `set_gi_proxy_disabled`).
    gi_proxy_off: HashSet<u64>,
    /// Where a counted mesh's brick reads its geometry (ADR-7721): a draw with no CPU vertices
    /// whose key the source holds is sized by its upload's box and baked from a readback.
    pub(crate) counted: Option<crate::asset_upload::CountedGiSource>,
    /// Free spans inside `pool_end`, sorted by offset and coalesced.
    free_spans: Vec<FreeSpan>,
    /// First value index not yet bumped. Freed tails shrink it.
    pool_end: u64,
    /// [`DdgiSettings::sdf_pool_values`] this state was created with.
    pool_capacity: u64,
    /// Bumped when a brick is freed. [`Self::unfit`] entries from an older
    /// generation are stale: free space may have grown.
    pool_gen: u64,
    /// Meshes that did not fit at `pool_gen`. Not a permanent failure.
    unfit: HashMap<u64, UnfitMemo>,
    /// Brick sizes, so a generation change can retest the fit without another vertex walk.
    brick_counts: HashMap<(u64, usize, usize), CachedBrickCount>,
    /// Times [`brick_value_count`] ran. A full pool must not increase this for a mesh it
    /// already refused.
    #[cfg(test)]
    fit_walks: u64,
    pool_full_logged: bool,
    /// Bricks waiting for upload: (first value, values).
    pub(crate) uploads: Vec<(u64, Vec<f32>)>,
    pub(crate) instances: Vec<SdfInstanceGpu>,
    pub(crate) lists: Vec<u32>,
    pub(crate) uniform: DdgiUniformGpu,
    /// Identity of the scene the probes last saw (bases, sun, instances).
    scene_key: u64,
    /// Frames since `scene_key` last changed.
    pub(crate) settled_frames: u32,
    /// The light of the previous frame (`relit`).
    last_light: Option<DdgiLight>,
    /// Candidate bits for this frame. Kept so a static frame does not allocate the bit vec.
    candidate_scratch: Vec<bool>,
    /// Instance spheres for the cascade lists (centre, radius), parallel to [`Self::instances`].
    sphere_scratch: Vec<(Vec3, f32)>,
    /// One cascade's chosen instances, sorted and truncated, then appended to [`Self::lists`].
    chosen_scratch: Vec<(f32, u32)>,
    /// Emitter ranking scratch ([`Self::fill_emitters`]).
    ranked_scratch: Vec<RankedEmitter>,
    /// World-space emitter geometry keyed by mesh, so a static emissive mesh is not transformed
    /// again on the next frame.
    emitter_cache: HashMap<u64, EmitterWorldMemo>,
    /// Times prepare replaced or grew the instance, list, emitter or sphere scratch.
    #[cfg(test)]
    scratch_growths: u64,
    /// Emitter clusters transformed into world space (a cache hit adds nothing).
    #[cfg(test)]
    emitter_transforms: u64,
    /// Times [`Self::retry_parked`] built its candidate set. Empty `parked` builds none.
    #[cfg(test)]
    parked_set_builds: u64,
    /// The instances of each instance set the probes list, by set key ([`Self::set_pick`]).
    set_picks: HashMap<u64, SetPick>,
}

/// The instances of one instance set the probes list, kept while what chose them stays.
struct SetPick {
    /// What the pick depends on besides the eye: the sets' generation, the instance count, the
    /// coarsest cascade's cell, the room in the instance buffer and the layout's size limits.
    stamp: u64,
    /// The finest cascade's cell when the pick was made: it ranks the instances when the set has
    /// more in reach than the buffer has room for (`ranked`).
    eye_base: [i32; 3],
    /// More instances were in reach than the buffer had room for.
    ranked: bool,
    /// Instance indices, ascending.
    picks: Vec<u32>,
}

impl DdgiState {
    pub(crate) fn new(settings: DdgiSettings) -> Self {
        let pool_capacity = settings.sdf_pool_values();
        Self {
            settings,
            frame: 0,
            bricks: HashMap::new(),
            pending: HashMap::new(),
            failed: HashSet::new(),
            albedo: HashMap::new(),
            emissive_map_mean: HashMap::new(),
            clusters: HashMap::new(),
            extents: HashMap::new(),
            emitters: Vec::new(),
            parked: HashMap::new(),
            gi_proxy_off: HashSet::new(),
            counted: None,
            free_spans: Vec::new(),
            pool_end: 0,
            pool_capacity,
            pool_gen: 0,
            unfit: HashMap::new(),
            brick_counts: HashMap::new(),
            #[cfg(test)]
            fit_walks: 0,
            pool_full_logged: false,
            uploads: Vec::new(),
            instances: Vec::new(),
            lists: Vec::new(),
            uniform: DdgiUniformGpu::default(),
            scene_key: 0,
            settled_frames: 0,
            last_light: None,
            candidate_scratch: Vec::new(),
            sphere_scratch: Vec::new(),
            chosen_scratch: Vec::new(),
            ranked_scratch: Vec::new(),
            emitter_cache: HashMap::new(),
            #[cfg(test)]
            scratch_growths: 0,
            #[cfg(test)]
            emitter_transforms: 0,
            #[cfg(test)]
            parked_set_builds: 0,
            set_picks: HashMap::new(),
        }
    }

    /// Forgets the probes (a camera cut): the next frame starts like a new state's first. The
    /// bricks, their pool and the albedo caches stay. Each brick's undrawn age is rebased onto
    /// the reset clock: the cut's own update counts as one undrawn update for a mesh that is
    /// absent, and a brick drawn on the previous update is not treated as ancient.
    pub(crate) fn restart(&mut self) {
        let frame = self.frame;
        self.frame = 0;
        for slot in self.bricks.values_mut() {
            let age = frame.wrapping_sub(slot.last_drawn);
            slot.last_drawn = 0u32.wrapping_sub(age);
        }
        self.instances.clear();
        self.lists.clear();
        self.emitters.clear();
        self.uniform = DdgiUniformGpu::default();
        self.scene_key = 0;
        self.settled_frames = 0;
        self.last_light = None;
    }

    /// The scene the probes trace changed in a way the settling key does not see (holes cut
    /// through the heightfield): the probes update again until they settle anew.
    pub(crate) fn unsettle(&mut self) {
        self.scene_key = 0;
        self.settled_frames = 0;
    }

    /// First word of each region of the probe data buffer: (irradiance, distance, shading
    /// headers, packed irradiance, screen lookup, end).
    pub(crate) fn data_words(settings: &DdgiSettings) -> (u64, u64, u64, u64, u64, u64) {
        let probes = settings.probe_count();
        let irr = probes * PROBE_WORDS;
        let vis = irr + probes * IRR_WORDS;
        let hdr = vis + probes * VIS_WORDS;
        let packed = hdr + probes * HEADER_WORDS;
        let screen = packed + probes * PACKED_IRR_WORDS;
        let (w, h) = settings.screen_region();
        let end = screen + u64::from(w) * u64::from(h) * SCREEN_WORDS;
        (irr, vis, hdr, packed, screen, end)
    }

    /// Sets the screen lookup for this frame: `frame` is the full (internal) frame size, `None`
    /// when the frame has no single-sample depth prepass (the lookup is then off), and
    /// `wanted` its resolution. A lookup larger than the region (`Full` above 1920 × 1080)
    /// falls back to half resolution, and below the default region (a tier's smaller
    /// [`DdgiSettings::screen_max`]) to quarter resolution after that. The uniform's `screen.w` is
    /// the step (pixels per texel).
    pub(crate) fn set_screen(&mut self, frame: Option<(u32, u32)>, wanted: GiLookupResolution) {
        let (_, _, _, _, base, _) = Self::data_words(&self.settings);
        let (max_w, max_h) = self.settings.screen_region();
        let fits = |w: u32, h: u32| w <= max_w && h <= max_h;
        let fitted = frame.and_then(|full| {
            fit_lookup(full, wanted, fits).or_else(|| {
                // The default region keeps its rule (ADR-1033): past it the lookup is off.
                if (max_w, max_h) == SCREEN_MAX {
                    return None;
                }
                let (w, h) = GiLookupResolution::Quarter.lookup_size(full.0, full.1);
                (w > 0 && h > 0 && fits(w, h)).then_some((w, h, GiLookupResolution::Quarter.step()))
            })
        });
        self.uniform.screen = match fitted {
            Some((w, h, step)) => [base as u32, w, h, step],
            None => [0; 4],
        };
    }

    /// Starts or collects brick bakes for the meshes this frame draws. With `sync` every
    /// brick is baked (or waited for) before the frame traces, so the frame does not depend
    /// on task-pool timing.
    ///
    /// `candidates` is this frame's candidate bits. The GI opt-out is applied on
    /// top of that bit, so neither check walks vertices.
    ///
    /// `sets` are the frame's instance sets and `set_wanted` is [`Self::set_candidates`] for them
    /// (ADR-5457): a set's mesh is baked, kept and retried like a placed mesh's.
    fn bake(
        &mut self,
        meshes: &[FrameMesh<'_>],
        candidates: &[bool],
        sets: &[ProbeSet<'_>],
        set_wanted: &[bool],
        sync: bool,
    ) {
        // Touch before any store: a brick drawn this frame is not eligible for eviction,
        // even when a bake that was already in flight needs the room.
        self.drop_opt_outs();
        self.touch_drawn(meshes, candidates);
        for (set, &wanted) in sets.iter().zip(set_wanted) {
            if wanted && let Some(slot) = self.bricks.get_mut(&set.mesh_key) {
                slot.last_drawn = self.frame;
            }
        }
        self.evict_stale();
        // Store in ascending mesh-key order, not task-pool finish order or `HashMap` order:
        // first-fit allocation is order-dependent (a different order parks a different mesh
        // or moves offsets), and the traced brick set must not depend on either.
        let mut finished: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, task)| sync || task.is_finished())
            .map(|(&k, _)| k)
            .collect();
        finished.sort_unstable();
        for key in finished {
            if let Some(task) = self.pending.remove(&key) {
                let brick = regia_core::tasks::block_on(task);
                self.store_brick(key, brick);
            }
        }
        self.retry_parked(meshes, candidates, sets, set_wanted);
        for (mesh, &candidate) in meshes.iter().zip(candidates) {
            if !self.proxy_bit(mesh, candidate) {
                continue;
            }
            if let Some(info) = self.counted_info(mesh) {
                self.request_counted_brick(mesh.mesh_key, info, sync);
                continue;
            }
            self.request_brick(
                mesh.mesh_key,
                mesh.vertices.len(),
                mesh.indices,
                &|| mesh.vertices.iter().map(|v| v.position).collect(),
                &|| mesh.vertices.iter().map(|v| v.normal).collect(),
                sync,
            );
        }
        for (set, &wanted) in sets.iter().zip(set_wanted) {
            if wanted {
                self.request_brick(
                    set.mesh_key,
                    set.positions.len(),
                    set.indices,
                    &|| set.positions.to_vec(),
                    &|| set.normals.to_vec(),
                    sync,
                );
            }
        }
    }

    /// Starts the bake of mesh `key`'s brick (or runs it, with `sync` or
    /// [`DdgiSettings::bake_on_caller`]) unless the brick is resident, in flight, failed, parked
    /// or known not to fit. `positions` and `normals` copy the mesh's vertices on the miss path
    /// only.
    fn request_brick(
        &mut self,
        key: u64,
        verts: usize,
        indices: &[u32],
        positions: &dyn Fn() -> Vec<[f32; 3]>,
        normals: &dyn Fn() -> Vec<[f32; 3]>,
        sync: bool,
    ) {
        if self.bricks.contains_key(&key)
            || self.pending.contains_key(&key)
            || self.failed.contains(&key)
            || self.parked.contains_key(&key)
        {
            return;
        }
        let index_count = indices.len();
        // Free space only shrinks until a brick is freed, so a refusal at this
        // generation is still true. The positions, normals and index copies, and
        // `brick_value_count`, stay on the miss path.
        if self.unfit_still(key, verts, index_count) {
            self.note_pool_full();
            return;
        }
        let Some(count) = self.cached_brick_count(key, verts, indices, positions) else {
            self.failed.insert(key);
            self.unfit.remove(&key);
            return;
        };
        if count > self.pool_capacity || !self.has_span(count) {
            if count > self.pool_capacity {
                self.failed.insert(key);
                self.unfit.remove(&key);
            } else {
                self.unfit.insert(
                    key,
                    UnfitMemo {
                        verts,
                        indices: index_count,
                        generation: self.pool_gen,
                    },
                );
            }
            self.note_pool_full();
            return;
        }
        self.unfit.remove(&key);
        let positions = positions();
        let normals = normals();
        let indices = indices.to_vec();
        if sync || self.settings.bake_on_caller {
            let brick = bake_brick_oriented(&positions, &normals, &indices);
            self.store_brick(key, brick);
        } else {
            self.pending.insert(
                key,
                regia_core::tasks::spawn(async move { bake_brick_oriented(&positions, &normals, &indices) }),
            );
        }
    }

    /// The upload's size-test inputs when `mesh` draws a resident counted mesh whole from its
    /// upload (no CPU vertices, ADR-7721).
    fn counted_info(&self, mesh: &FrameMesh<'_>) -> Option<crate::asset_upload::CountedProxyInfo> {
        if !mesh.vertices.is_empty() || !crate::is_asset_mesh_key(mesh.mesh_key) {
            return None;
        }
        self.counted.as_ref()?.info(mesh.mesh_key)
    }

    /// [`Self::request_brick`] for a counted mesh (ADR-7721): its geometry is a GPU readback of
    /// its upload, started on the brick's miss path only. A readback still in flight retries on a
    /// later frame (with `sync` it is waited for).
    fn request_counted_brick(&mut self, key: u64, info: crate::asset_upload::CountedProxyInfo, sync: bool) {
        if self.bricks.contains_key(&key)
            || self.pending.contains_key(&key)
            || self.failed.contains(&key)
            || self.parked.contains_key(&key)
            || self.unfit_still(key, info.vertices, info.indices)
        {
            return;
        }
        let Some(source) = self.counted.as_ref() else {
            return;
        };
        let wait = sync || self.settings.bake_on_caller;
        let Some(geometry) = source.fetch(key, wait) else {
            return;
        };
        let crate::asset_upload::CountedGiGeometry {
            positions,
            normals,
            indices,
        } = geometry;
        self.request_brick(
            key,
            positions.len(),
            &indices,
            &|| positions.clone(),
            &|| normals.clone(),
            sync,
        );
    }

    fn evict_after(&self) -> u32 {
        self.settings.sdf_evict_frames.max(1)
    }

    /// Size test from the candidate bit, then the GI opt-out. No vertex walk.
    fn proxy_bit(&self, mesh: &FrameMesh<'_>, candidate: bool) -> bool {
        candidate && !self.gi_proxy_off.contains(&mesh.mesh_key)
    }

    /// Whether `key` was refused at the current pool generation with this geometry.
    fn unfit_still(&mut self, key: u64, verts: usize, indices: usize) -> bool {
        let Some(memo) = self.unfit.get(&key) else {
            return false;
        };
        if memo.generation != self.pool_gen || memo.verts != verts || memo.indices != indices {
            return false;
        }
        if let Some(hit) = self.brick_counts.get_mut(&(key, verts, indices)) {
            hit.seen = self.frame;
        }
        true
    }

    /// Brick size for mesh `key`, walking its vertices (`positions`) only on a cache miss.
    fn cached_brick_count(
        &mut self,
        key: u64,
        verts: usize,
        indices: &[u32],
        positions: &dyn Fn() -> Vec<[f32; 3]>,
    ) -> Option<u64> {
        let key = (key, verts, indices.len());
        if let Some(hit) = self.brick_counts.get_mut(&key) {
            hit.seen = self.frame;
            return hit.count;
        }
        let count = brick_value_count(&positions(), indices);
        #[cfg(test)]
        {
            self.fit_walks = self.fit_walks.wrapping_add(1);
        }
        self.brick_counts.insert(
            key,
            CachedBrickCount {
                count,
                seen: self.frame,
            },
        );
        count
    }

    /// A brick whose mesh has not been drawn for [`Self::evict_after`] frames.
    fn brick_evictable(&self, slot: &BrickSlot) -> bool {
        slot.last_drawn != self.frame && self.frame.wrapping_sub(slot.last_drawn) >= self.evict_after()
    }

    fn drop_opt_outs(&mut self) {
        if self.gi_proxy_off.is_empty() {
            return;
        }
        let keys: Vec<u64> = self.gi_proxy_off.iter().copied().collect();
        for key in keys {
            self.free_brick(key);
            self.parked.remove(&key);
        }
    }

    fn touch_drawn(&mut self, meshes: &[FrameMesh<'_>], candidates: &[bool]) {
        for (mesh, &candidate) in meshes.iter().zip(candidates) {
            if !self.proxy_bit(mesh, candidate) {
                continue;
            }
            if let Some(slot) = self.bricks.get_mut(&mesh.mesh_key) {
                slot.last_drawn = self.frame;
            }
        }
    }

    fn evict_stale(&mut self) {
        let stale: Vec<u64> = self
            .bricks
            .iter()
            .filter(|(_, slot)| self.brick_evictable(slot))
            .map(|(&key, _)| key)
            .collect();
        for key in stale {
            self.free_brick(key);
        }
    }

    fn retry_parked(
        &mut self,
        meshes: &[FrameMesh<'_>],
        candidates: &[bool],
        sets: &[ProbeSet<'_>],
        set_wanted: &[bool],
    ) {
        if self.parked.is_empty() {
            return;
        }
        #[cfg(test)]
        {
            self.parked_set_builds += 1;
        }
        let wanted: HashSet<u64> = meshes
            .iter()
            .zip(candidates.iter().copied())
            .filter(|(mesh, candidate)| self.proxy_bit(mesh, *candidate))
            .map(|(mesh, _)| mesh.mesh_key)
            .chain(
                sets.iter()
                    .zip(set_wanted)
                    .filter(|(_, wanted)| **wanted)
                    .map(|(set, _)| set.mesh_key),
            )
            .collect();
        let keys: Vec<u64> = self.parked.keys().copied().collect();
        for key in keys {
            let Some(brick) = self.parked.remove(&key) else {
                continue;
            };
            if wanted.contains(&key) {
                self.store_brick(key, Some(brick));
            }
        }
    }

    fn note_pool_full(&mut self) {
        if !self.pool_full_logged {
            tracing::warn!("ddgi: signed-distance brick pool full; later meshes are not traced");
            self.pool_full_logged = true;
        }
    }

    fn has_span(&self, len: u64) -> bool {
        len > 0
            && (self.free_spans.iter().any(|span| span.len >= len)
                || self.pool_end.saturating_add(len) <= self.pool_capacity)
    }

    /// Forgets meshes whose last `Arc` dropped ([`crate::FrameRenderer::release_meshes`]): their
    /// bricks return to the pool now, and the per-key memos without idle eviction (emitter
    /// clusters, failed bakes) go too. Generation keys never come back, so nothing else would
    /// drop them.
    pub(crate) fn release_meshes(&mut self, keys: &[u64]) {
        for &key in keys {
            self.free_brick(key);
            self.pending.remove(&key);
            self.failed.remove(&key);
            self.parked.remove(&key);
            self.clusters.remove(&key);
            self.emitter_cache.remove(&key);
            self.unfit.remove(&key);
        }
    }

    fn free_brick(&mut self, key: u64) {
        let Some(slot) = self.bricks.remove(&key) else {
            return;
        };
        insert_free(
            &mut self.free_spans,
            u64::from(slot.offset),
            u64::from(slot.len),
            &mut self.pool_end,
        );
        // Refusals were about the old free list. A later mesh can use this span.
        self.pool_gen = self.pool_gen.wrapping_add(1);
    }

    /// A contiguous span of `len` values, evicting least-recently-drawn eligible bricks until
    /// one exists. `None` when every remaining brick was drawn inside the evict window.
    fn alloc_span(&mut self, len: u64) -> Option<u64> {
        if let Some(offset) = take_span(&mut self.free_spans, &mut self.pool_end, self.pool_capacity, len) {
            return Some(offset);
        }
        loop {
            let key = self
                .bricks
                .iter()
                .filter(|(_, slot)| self.brick_evictable(slot))
                .min_by_key(|(key, slot)| (slot.last_drawn, *key))
                .map(|(key, _)| *key)?;
            self.free_brick(key);
            if let Some(offset) = take_span(&mut self.free_spans, &mut self.pool_end, self.pool_capacity, len)
            {
                return Some(offset);
            }
        }
    }

    fn store_brick(&mut self, key: u64, brick: Option<SdfBrick>) {
        if self.gi_proxy_off.contains(&key) {
            return;
        }
        let Some(brick) = brick else {
            self.failed.insert(key);
            self.parked.remove(&key);
            return;
        };
        let len = brick.values.len() as u64;
        if len == 0 || len > self.pool_capacity {
            self.failed.insert(key);
            return;
        }
        let Some(offset) = self.alloc_span(len) else {
            self.parked.insert(key, brick);
            self.note_pool_full();
            return;
        };
        let (Some(offset_u32), Some(len_u32)) = (u32::try_from(offset).ok(), u32::try_from(len).ok()) else {
            insert_free(&mut self.free_spans, offset, len, &mut self.pool_end);
            self.failed.insert(key);
            return;
        };
        self.pool_full_logged = false;
        self.parked.remove(&key);
        self.failed.remove(&key);
        self.bricks.insert(
            key,
            BrickSlot {
                offset: offset_u32,
                len: len_u32,
                origin: brick.origin,
                voxel: brick.voxel,
                dims: brick.dims,
                last_drawn: self.frame,
            },
        );
        self.uploads.push((offset, brick.values));
    }

    fn mean_albedo(&mut self, mesh: &FrameMesh<'_>) -> [f32; 3] {
        self.mean_albedo_of(mesh.material, mesh.material_key)
    }

    /// The mean albedo the probe rays see on a surface of `material` (cached per material key).
    fn mean_albedo_of(&mut self, material: &crate::LabMaterial, material_key: u64) -> [f32; 3] {
        let base = material.base_color;
        let tex = match map_texels(material.albedo.as_ref(), material, 0) {
            Some(t) if material_key != 0 => *self
                .albedo
                .entry(material_key)
                .or_insert_with(|| mean_linear_rgb(&t.rgba).unwrap_or([1.0; 3])),
            Some(t) => mean_linear_rgb(&t.rgba).unwrap_or([1.0; 3]),
            None => [1.0; 3],
        };
        std::array::from_fn(|c| (base[c] * tex[c]).clamp(0.0, 0.95))
    }

    /// A mesh's emitted radiance as the probe rays see it: factor × strength × the emissive
    /// map's mean (trim and strips are usually a mask in the map; the factor alone lit the
    /// whole mesh).
    fn mean_emissive(&mut self, mesh: &FrameMesh<'_>) -> [f32; 3] {
        self.mean_emissive_of(mesh.material, mesh.material_key)
    }

    /// [`Self::mean_emissive`] for a material and its key.
    fn mean_emissive_of(&mut self, m: &crate::LabMaterial, material_key: u64) -> [f32; 3] {
        let tex = match map_texels(m.emissive_map.as_ref(), m, 3) {
            Some(t) if material_key != 0 => *self
                .emissive_map_mean
                .entry(material_key)
                .or_insert_with(|| mean_linear_rgb(&t.rgba).unwrap_or([1.0; 3])),
            Some(t) => mean_linear_rgb(&t.rgba).unwrap_or([1.0; 3]),
            None => [1.0; 3],
        };
        std::array::from_fn(|c| m.emissive[c] * m.emissive_intensity * tex[c])
    }

    /// CPU work for one frame without instance sets ([`Self::prepare_with_sets`]).
    #[cfg(test)]
    pub(crate) fn prepare(
        &mut self,
        eye: [f32; 3],
        light: &DdgiLight,
        meshes: &[FrameMesh<'_>],
        local_ranges: &[f32],
        heightfield: bool,
        sync_bakes: bool,
    ) {
        self.prepare_with_sets(
            eye,
            light,
            &ProbeGeometry { meshes, sets: &[] },
            local_ranges,
            heightfield,
            sync_bakes,
        );
    }

    /// Which of the frame's instance sets get a brick (ADR-5457): a set that is eligible, not
    /// opted out, and has an instance inside the size limits of [`proxy_fits`] (the largest
    /// instance for the least radius, the smallest for the coarsest voxel). The mesh's bounds
    /// are walked once per `(mesh key, vertex count)`, as for a placed mesh.
    fn set_candidates(&mut self, sets: &[ProbeSet<'_>]) -> Vec<bool> {
        let frame = self.frame;
        sets.iter()
            .map(|set| {
                if !set.eligible() || self.gi_proxy_off.contains(&set.mesh_key) {
                    return false;
                }
                let extent = self
                    .extents
                    .entry((set.mesh_key, set.positions.len()))
                    .or_insert_with(|| MeshExtent::of_positions(set.positions, frame));
                extent.seen = frame;
                let size = extent.hi - extent.lo;
                let s = &self.settings;
                let small = size * set.scale_range.0;
                (size * set.scale_range.1).length() * 0.5 >= s.min_proxy_radius
                    && (small.min_element() <= s.max_proxy_voxel / 8.0
                        || small.max_element() / crate::ddgi_bake::MAX_BRICK_DIM as f32 <= s.max_proxy_voxel)
            })
            .collect()
    }

    /// The instances of `set` the probes list (ADR-5457), as ascending indices, and whether the
    /// set had more of them in reach than `room`. An instance is listed when it casts a shadow,
    /// passes a placed mesh's size test ([`proxy_fits`]) and its bounding sphere touches `reach`
    /// (the box a cascade's rays can reach: the coarsest cascade's volume plus its reach holds
    /// every finer one's). Past `room` the best by the cascade lists' score stay, radius over
    /// distance from `rank_from`.
    fn set_pick(
        set: &ProbeSet<'_>,
        extent: &MeshExtent,
        settings: &DdgiSettings,
        reach: (Vec3, Vec3),
        rank_from: Vec3,
        room: usize,
    ) -> (Vec<u32>, bool) {
        let (lo_all, hi_all) = reach;
        let centre_l = (extent.lo + extent.hi) * 0.5;
        let radius_l = (extent.hi - extent.lo).length() * 0.5;
        let mut chosen: Vec<(f32, u32)> = Vec::new();
        for (i, model) in set.models.iter().enumerate() {
            if set.shadow_of(i) == ShadowCast::Off || !proxy_fits(model, extent.lo, extent.hi, settings) {
                continue;
            }
            let m = Mat4::from_cols_array_2d(model);
            let scale = m
                .x_axis
                .truncate()
                .length()
                .max(m.y_axis.truncate().length())
                .max(m.z_axis.truncate().length());
            let (centre, radius) = (m.transform_point3(centre_l), radius_l * scale);
            if (0..3).all(|a| centre[a] + radius >= lo_all[a] && centre[a] - radius <= hi_all[a]) {
                chosen.push((-(radius / (centre.distance(rank_from) + 1.0)), i as u32));
            }
        }
        let ranked = chosen.len() > room;
        if ranked {
            chosen.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            chosen.truncate(room);
            chosen.sort_by_key(|c| c.1);
        }
        (chosen.into_iter().map(|(_, i)| i).collect(), ranked)
    }

    /// CPU work for one frame: bricks for the frame's meshes and instance sets, the cascades,
    /// the instance lists and the uniform.
    pub(crate) fn prepare_with_sets(
        &mut self,
        eye: [f32; 3],
        light: &DdgiLight,
        geometry: &ProbeGeometry<'_, '_>,
        local_ranges: &[f32],
        heightfield: bool,
        sync_bakes: bool,
    ) {
        let ProbeGeometry { meshes, sets } = *geometry;
        let _span = tracing::info_span!("ddgi prepare").entered();
        self.frame = self.frame.wrapping_add(1).max(1);
        let relit = self
            .last_light
            .as_ref()
            .is_some_and(|last| light_cut(last, light));
        self.last_light = Some(*light);
        #[cfg(test)]
        let scratch_before = (
            self.instances.capacity(),
            self.lists.capacity(),
            self.emitters.capacity(),
            self.sphere_scratch.capacity(),
            self.chosen_scratch.capacity(),
            self.candidate_scratch.capacity(),
        );
        self.fill_candidates(meshes);
        let candidates = std::mem::take(&mut self.candidate_scratch);
        let set_wanted = self.set_candidates(sets);
        self.bake(meshes, &candidates, sets, &set_wanted, sync_bakes);
        // `clamped` replaces a non-positive spacing before any cascade divides by it.
        let s = self.settings.clone().clamped();
        let s = &s;
        let cascades = s.cascades.clamp(1, MAX_CASCADES as u32);
        let mut uniform = DdgiUniformGpu::default();
        let mut bases = [[0i32; 3]; MAX_CASCADES];
        for (c, (slot, gpu)) in bases
            .iter_mut()
            .zip(uniform.cascades.iter_mut())
            .take(cascades as usize)
            .enumerate()
        {
            let spacing = s.spacing_of(c as u32);
            let lift = cascade_lift_cells(s, c as u32);
            let base = cascade_base(eye, spacing, s.dims, lift);
            let origin = cascade_origin(eye, spacing, s.dims, lift);
            *slot = base;
            *gpu = CascadeGpu {
                origin: [origin[0], origin[1], origin[2], spacing],
                base: [base[0], base[1], base[2], c as i32],
            };
        }
        // Instances: every rigid mesh with a brick; per cascade the ones its rays can reach.
        self.instances.clear();
        self.sphere_scratch.clear();
        for (mesh, &candidate) in meshes.iter().zip(&candidates) {
            if self.instances.len() >= MAX_INSTANCES || !self.proxy_bit(mesh, candidate) {
                continue;
            }
            let Some(slot) = self.bricks.get(&mesh.mesh_key).copied() else {
                continue;
            };
            let model = Mat4::from_cols_array_2d(&mesh.model);
            let inv = model.inverse();
            if !inv.is_finite() {
                continue;
            }
            let lo = Vec3::from_array(slot.origin);
            let hi = lo
                + Vec3::new(
                    slot.voxel[0] * (slot.dims[0] - 1) as f32,
                    slot.voxel[1] * (slot.dims[1] - 1) as f32,
                    slot.voxel[2] * (slot.dims[2] - 1) as f32,
                );
            let (wmin, wmax) = transform_aabb(&model, lo, hi);
            let rows = inv.transpose().to_cols_array_2d();
            let eps = 0.5 * slot.voxel.iter().copied().fold(f32::MAX, f32::min);
            let albedo = self.mean_albedo(mesh);
            // With the emitter list on, the mesh's light reaches the probes through it.
            let emissive = if s.emitters {
                [0.0; 3]
            } else {
                self.mean_emissive(mesh)
            };
            self.instances.push(SdfInstanceGpu {
                w2l: [rows[0], rows[1], rows[2]],
                aabb_min: [wmin.x, wmin.y, wmin.z, eps],
                aabb_max: [wmax.x, wmax.y, wmax.z, 0.0],
                brick_origin: [slot.origin[0], slot.origin[1], slot.origin[2], 0.0],
                brick_voxel: [slot.voxel[0], slot.voxel[1], slot.voxel[2], 0.0],
                brick: [slot.offset, slot.dims[0], slot.dims[1], slot.dims[2]],
                albedo: [albedo[0], albedo[1], albedo[2], hit_roughness(mesh.material)],
                emissive: [emissive[0], emissive[1], emissive[2], hit_metallic(mesh.material)],
            });
            self.sphere_scratch
                .push(((wmin + wmax) * 0.5, (wmax - wmin).length() * 0.5));
        }
        self.lists.clear();
        let eye_v = Vec3::from_array(eye);
        // Instance sets (ADR-5457): each instance enters as a placed mesh of the set's mesh,
        // material and model would. Only instances a cascade's rays can reach are listed (the
        // coarsest cascade's volume plus its reach holds every finer one's), and when a set has
        // more of them than the instance buffer has room for, the largest at the eye stay.
        if !sets.is_empty() {
            let last = cascades as usize - 1;
            let spacing = s.spacing_of(last as u32);
            let reach = spacing * s.dims.iter().copied().max().unwrap_or(1) as f32;
            let base = bases[last];
            let lo_all =
                Vec3::new(base[0] as f32, base[1] as f32, base[2] as f32) * spacing - Vec3::splat(reach);
            let hi_all = Vec3::new(
                (base[0] + s.dims[0] as i32 - 1) as f32,
                (base[1] + s.dims[1] as i32 - 1) as f32,
                (base[2] + s.dims[2] as i32 - 1) as f32,
            ) * spacing
                + Vec3::splat(reach);
            // The finest cascade's middle, in whole cells: what ranks a crowded set's instances.
            let fine = s.spacing_of(0);
            let rank_from = Vec3::new(
                (bases[0][0] + s.dims[0] as i32 / 2) as f32,
                (bases[0][1] + s.dims[1] as i32 / 2) as f32,
                (bases[0][2] + s.dims[2] as i32 / 2) as f32,
            ) * fine;
            for (set, &wanted) in sets.iter().zip(&set_wanted) {
                let room = MAX_INSTANCES.saturating_sub(self.instances.len());
                if !wanted || room == 0 {
                    continue;
                }
                let (Some(slot), Some(extent)) = (
                    self.bricks.get(&set.mesh_key).copied(),
                    self.extents.get(&(set.mesh_key, set.positions.len())).copied(),
                ) else {
                    continue;
                };
                // What the pick depends on: any of these moving picks again (a pass over the set).
                let mut stamp = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hash::hash(
                    &(
                        set.generation,
                        set.models.len(),
                        bases[last],
                        room,
                        (s.dims, cascades),
                        [
                            s.spacing.to_bits(),
                            s.spacing_ratio.to_bits(),
                            s.min_proxy_radius.to_bits(),
                            s.max_proxy_voxel.to_bits(),
                        ],
                    ),
                    &mut stamp,
                );
                let stamp = std::hash::Hasher::finish(&stamp);
                let kept = self
                    .set_picks
                    .remove(&set.set_key)
                    .filter(|pick| pick.stamp == stamp && (!pick.ranked || pick.eye_base == bases[0]));
                let pick = kept.unwrap_or_else(|| {
                    let (picks, ranked) = Self::set_pick(set, &extent, s, (lo_all, hi_all), rank_from, room);
                    SetPick {
                        stamp,
                        eye_base: bases[0],
                        ranked,
                        picks,
                    }
                });
                if pick.picks.is_empty() {
                    self.set_picks.insert(set.set_key, pick);
                    continue;
                }
                let lo = Vec3::from_array(slot.origin);
                let hi = lo
                    + Vec3::new(
                        slot.voxel[0] * (slot.dims[0] - 1) as f32,
                        slot.voxel[1] * (slot.dims[1] - 1) as f32,
                        slot.voxel[2] * (slot.dims[2] - 1) as f32,
                    );
                let albedo = self.mean_albedo_of(set.material, set.material_key);
                // A set has no entry in the emitter list: its emission reaches the probes at ray
                // hits on its brick.
                let emissive = self.mean_emissive_of(set.material, set.material_key);
                let eps = 0.5 * slot.voxel.iter().copied().fold(f32::MAX, f32::min);
                for &i in &pick.picks {
                    let model = Mat4::from_cols_array_2d(&set.models[i as usize]);
                    let inv = model.inverse();
                    if !inv.is_finite() {
                        continue;
                    }
                    let (wmin, wmax) = transform_aabb(&model, lo, hi);
                    let rows = inv.transpose().to_cols_array_2d();
                    self.instances.push(SdfInstanceGpu {
                        w2l: [rows[0], rows[1], rows[2]],
                        aabb_min: [wmin.x, wmin.y, wmin.z, eps],
                        aabb_max: [wmax.x, wmax.y, wmax.z, 0.0],
                        brick_origin: [slot.origin[0], slot.origin[1], slot.origin[2], 0.0],
                        brick_voxel: [slot.voxel[0], slot.voxel[1], slot.voxel[2], 0.0],
                        brick: [slot.offset, slot.dims[0], slot.dims[1], slot.dims[2]],
                        albedo: [albedo[0], albedo[1], albedo[2], hit_roughness(set.material)],
                        emissive: [emissive[0], emissive[1], emissive[2], hit_metallic(set.material)],
                    });
                    self.sphere_scratch
                        .push(((wmin + wmax) * 0.5, (wmax - wmin).length() * 0.5));
                }
                self.set_picks.insert(set.set_key, pick);
            }
        }
        // The picks of sets that are gone (or no longer traced) go with them.
        if self.set_picks.len() > sets.len() {
            self.set_picks
                .retain(|key, _| sets.iter().any(|set| set.set_key == *key));
        }
        for (c, base) in bases.iter().take(cascades as usize).enumerate() {
            let spacing = s.spacing_of(c as u32);
            let reach = spacing * s.dims.iter().copied().max().unwrap_or(1) as f32;
            let lo = Vec3::new(base[0] as f32, base[1] as f32, base[2] as f32) * spacing - Vec3::splat(reach);
            let hi = Vec3::new(
                cascade_hi_cell(base[0], s.dims[0]) as f32,
                cascade_hi_cell(base[1], s.dims[1]) as f32,
                cascade_hi_cell(base[2], s.dims[2]) as f32,
            ) * spacing
                + Vec3::splat(reach);
            self.chosen_scratch.clear();
            let spheres = self.sphere_scratch.len();
            let mut sphere_i = 0;
            while sphere_i < spheres {
                let (centre, radius) = self.sphere_scratch[sphere_i];
                let index = sphere_i as u32;
                sphere_i += 1;
                if (0..3).all(|a| centre[a] + radius >= lo[a] && centre[a] - radius <= hi[a]) {
                    self.chosen_scratch
                        .push((-(radius / (centre.distance(eye_v) + 1.0)), index));
                }
            }
            self.chosen_scratch
                .sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            self.chosen_scratch.truncate(s.max_instances_per_cascade as usize);
            uniform.inst_offset[c] = self.lists.len() as u32;
            uniform.inst_count[c] = self.chosen_scratch.len() as u32;
            let chosen = self.chosen_scratch.len();
            let mut chosen_i = 0;
            while chosen_i < chosen {
                let index = self.chosen_scratch[chosen_i].1;
                chosen_i += 1;
                self.lists.push(index);
            }
        }
        let (irr, vis, hdr, packed, _, _) = Self::data_words(s);
        // One independent rotation a frame. Two quasi-random sequences were measured and dropped (ADR-5457):
        // a probe takes every 8th, 16th, ... frame's rotation, and at such a stride an additive recurrence's
        // points covered the cube worse than independent numbers, while a Halton coordinate whose base
        // divides the stride minus one (8 and 7, 16 and 5) climbs in even steps, so a probe's consecutive
        // rotations were alike.
        let rot = random_rotation([
            hash01(self.frame.wrapping_mul(3)),
            hash01(self.frame.wrapping_mul(3) + 1),
            hash01(self.frame.wrapping_mul(3) + 2),
        ]);
        uniform.dims = [s.dims[0], s.dims[1], s.dims[2], cascades];
        uniform.params = [s.hysteresis, s.normal_bias, s.view_bias, s.intensity];
        uniform.ctrl = [
            RAYS_PER_PROBE,
            self.frame,
            packed_update_divisor(s),
            u32::from(!s.shading_off_debug)
                | if heightfield { 2 } else { 0 }
                | if s.screen_exact_debug { 4 } else { 0 }
                | (update_cadence(s).1 << 8)
                | if relit { 1 << 12 } else { 0 }
                | if s.needs_room_rule() {
                    CTRL_RAY_KEEPS_CASCADE
                } else {
                    0
                }
                | s.specular.ctrl_bits()
                | s.seed_blend_bits()
                | if s.front_weights { CTRL_FRONT_WEIGHTS } else { 0 }
                | if s.edge_visibility {
                    CTRL_EDGE_VISIBILITY
                } else {
                    0
                }
                | if light.ground.is_some() {
                    CTRL_GROUND_PLANE
                } else {
                    0
                },
        ];
        // The ground plane's numbers (`CTRL_GROUND_PLANE`): height, radius and centre x in the
        // rotation rows' fourth components, centre z in `sun.w`, the radiance in `fill.w` and
        // `emitters.zw` (as bits).
        let (plane, plane_light) = light.ground.unwrap_or(([0.0; 4], [0.0; 3]));
        uniform.rot = [
            [rot[0][0], rot[0][1], rot[0][2], plane[0]],
            [rot[1][0], rot[1][1], rot[1][2], plane[1]],
            [rot[2][0], rot[2][1], rot[2][2], plane[2]],
        ];
        uniform.to_sun = match light.to_sun {
            Some(d) => [d[0], d[1], d[2], 1.0],
            None => [0.0, 1.0, 0.0, 0.0],
        };
        uniform.sun = [light.sun[0], light.sun[1], light.sun[2], plane[3]];
        uniform.sky = [
            light.sky_scale,
            light.sky_lod,
            s.max_terrain_ray,
            s.visibility_limit,
        ];
        uniform.flat_sky = [
            light.flat_sky[0],
            light.flat_sky[1],
            light.flat_sky[2],
            s.min_frontface,
        ];
        (uniform.to_fill, uniform.fill) = match light.fill {
            Some([d, c]) => ([d[0], d[1], d[2], 1.0], [c[0], c[1], c[2], plane_light[0]]),
            None => ([0.0; 4], [0.0, 0.0, 0.0, plane_light[0]]),
        };
        for (i, light) in light.extra.iter().enumerate() {
            if let Some([d, c]) = light {
                uniform.extra_dir[2 * i] = [d[0], d[1], d[2], 1.0];
                uniform.extra_dir[2 * i + 1] = [c[0], c[1], c[2], 0.0];
            }
        }
        uniform.words = [irr as u32, vis as u32, hdr as u32, packed as u32];
        if s.emitters {
            self.fill_emitters(eye_v, meshes, local_ranges);
        } else {
            self.emitters.clear();
        }
        uniform.emitters = [
            self.emitters.len() as u32,
            s.emitter_local_range.max(0.0).to_bits(),
            plane_light[1].to_bits(),
            plane_light[2].to_bits(),
        ];
        // Settling: the probes stop changing once the scene they trace is unchanged.
        let mut key = std::collections::hash_map::DefaultHasher::new();
        // The snapped bases and spacings only: the unsnapped corner (`origin.xyz`) moves the
        // shading's blend window, not what the probes trace.
        for gpu in &uniform.cascades {
            std::hash::Hasher::write(&mut key, bytemuck::bytes_of(&gpu.base));
            std::hash::Hasher::write(&mut key, &gpu.origin[3].to_bits().to_le_bytes());
        }
        std::hash::Hasher::write(&mut key, bytemuck::bytes_of(&uniform.to_sun));
        std::hash::Hasher::write(&mut key, bytemuck::bytes_of(&uniform.sun));
        std::hash::Hasher::write(&mut key, bytemuck::bytes_of(&uniform.sky));
        std::hash::Hasher::write(&mut key, bytemuck::bytes_of(&uniform.fill));
        std::hash::Hasher::write(&mut key, bytemuck::bytes_of(&uniform.to_fill));
        // The ground plane: on or off, where it lies and its radiance (`sun.w` and `fill.w` above
        // hold two of its numbers already; the rotation rows change every frame and stay out).
        std::hash::Hasher::write(&mut key, &[u8::from(light.ground.is_some())]);
        std::hash::Hasher::write(&mut key, bytemuck::bytes_of(&plane));
        std::hash::Hasher::write(&mut key, bytemuck::bytes_of(&plane_light));
        std::hash::Hasher::write(&mut key, bytemuck::bytes_of(&uniform.extra_dir));
        std::hash::Hasher::write(&mut key, bytemuck::cast_slice(&self.instances));
        std::hash::Hasher::write(&mut key, bytemuck::cast_slice(&self.emitters));
        let key = std::hash::Hasher::finish(&key);
        if key == self.scene_key {
            self.settled_frames = self.settled_frames.saturating_add(1);
        } else {
            self.scene_key = key;
            self.settled_frames = 0;
        }
        self.uniform = uniform;
        self.candidate_scratch = candidates;
        #[cfg(test)]
        {
            let grew = self.instances.capacity() > scratch_before.0
                || self.lists.capacity() > scratch_before.1
                || self.emitters.capacity() > scratch_before.2
                || self.sphere_scratch.capacity() > scratch_before.3
                || self.chosen_scratch.capacity() > scratch_before.4
                || self.candidate_scratch.capacity() > scratch_before.5;
            if grew {
                self.scratch_growths += 1;
            }
        }
    }

    /// The frame's emitter list: every rigid emissive mesh's clusters, the strongest at the
    /// eye first, at most [`MAX_EMITTERS`]. Bitwise-equal strengths keep the earlier mesh,
    /// then the lower cell coordinate, so the cap survives the same emitters in every
    /// process. `local_ranges[i]` > 0: mesh `i`'s emission is lit
    /// out to that range by the short-range emissive pass, so its clusters carry the range and
    /// the probes take only the far part.
    #[cfg(test)]
    fn emitter_list(&mut self, eye: Vec3, meshes: &[FrameMesh<'_>], local_ranges: &[f32]) -> Vec<EmitterGpu> {
        self.fill_emitters(eye, meshes, local_ranges);
        self.emitters.clone()
    }

    /// Fills [`Self::emitters`] from the cached world clusters, recomputing strength at `eye`.
    fn fill_emitters(&mut self, eye: Vec3, meshes: &[FrameMesh<'_>], local_ranges: &[f32]) {
        self.ranked_scratch.clear();
        for (mesh_index, mesh) in meshes.iter().enumerate() {
            if mesh.mesh_key == 0 || !mesh.joints.is_empty() || mesh.indices.len() < 3 {
                continue;
            }
            let radiance = self.mean_emissive(mesh);
            let luminance = 0.2126 * radiance[0] + 0.7152 * radiance[1] + 0.0722 * radiance[2];
            if luminance <= 1e-3 {
                continue;
            }
            let model = Mat4::from_cols_array_2d(&mesh.model);
            let model_bits = model.to_cols_array().map(f32::to_bits);
            let radiance_bits = radiance.map(f32::to_bits);
            let local_range = local_ranges.get(mesh_index).copied().unwrap_or(0.0).max(0.0);
            let local_range_bits = local_range.to_bits();
            let cached = self.emitter_cache.get_mut(&mesh.mesh_key).and_then(|memo| {
                (memo.model_bits == model_bits
                    && memo.radiance_bits == radiance_bits
                    && memo.local_range_bits == local_range_bits)
                    .then(|| std::mem::take(&mut memo.records))
            });
            if let Some(records) = cached {
                for rec in &records {
                    let strength =
                        rec.strength_num / (rec.centre.distance_squared(eye) + rec.radius * rec.radius);
                    self.ranked_scratch.push(RankedEmitter {
                        strength,
                        mesh_index,
                        cell: rec.cell,
                        emitter: rec.emitter,
                    });
                }
                if let Some(memo) = self.emitter_cache.get_mut(&mesh.mesh_key) {
                    memo.records = records;
                }
                continue;
            }
            let clusters = std::mem::take(
                self.clusters
                    .entry(mesh.mesh_key)
                    .or_insert_with(|| emitter_clusters(mesh)),
            );
            #[cfg(test)]
            {
                self.emitter_transforms = self.emitter_transforms.saturating_add(clusters.len() as u64);
            }
            let scale = model
                .x_axis
                .truncate()
                .length()
                .max(model.y_axis.truncate().length())
                .max(model.z_axis.truncate().length());
            // Normals go through the inverse transpose (a non-uniform scale tilts them).
            let normal_matrix = model.inverse().transpose();
            let mut records = Vec::with_capacity(clusters.len());
            for c in &clusters {
                let centre = model.transform_point3(c.centroid);
                let area = c.area * scale * scale;
                let radius = (c.half_diagonal * scale).max(0.01);
                let flat = (c.normal_sum.length() / c.area).clamp(0.0, 1.0);
                let normal = normal_matrix.transform_vector3(c.normal_sum).normalize_or_zero();
                let power: [f32; 3] = std::array::from_fn(|k| radiance[k] * area);
                let strength_num = luminance * area;
                let strength = strength_num / (centre.distance_squared(eye) + radius * radius);
                let emitter = EmitterGpu {
                    pos_radius: [centre.x, centre.y, centre.z, radius],
                    power: [power[0], power[1], power[2], local_range],
                    normal_flat: [normal.x, normal.y, normal.z, flat],
                };
                records.push(EmitterWorldRecord {
                    cell: c.cell,
                    centre,
                    radius,
                    strength_num,
                    emitter,
                });
                self.ranked_scratch.push(RankedEmitter {
                    strength,
                    mesh_index,
                    cell: c.cell,
                    emitter,
                });
            }
            self.clusters.insert(mesh.mesh_key, clusters);
            self.emitter_cache.insert(
                mesh.mesh_key,
                EmitterWorldMemo {
                    model_bits,
                    radiance_bits,
                    local_range_bits,
                    records,
                },
            );
        }
        self.ranked_scratch.sort_by(|a, b| {
            b.strength
                .total_cmp(&a.strength)
                .then_with(|| a.mesh_index.cmp(&b.mesh_index))
                .then_with(|| a.cell.cmp(&b.cell))
        });
        self.ranked_scratch.truncate(MAX_EMITTERS);
        self.emitters.clear();
        let ranked = self.ranked_scratch.len();
        let mut ranked_i = 0;
        while ranked_i < ranked {
            let emitter = self.ranked_scratch[ranked_i].emitter;
            ranked_i += 1;
            self.emitters.push(emitter);
        }
    }

    /// Whether the screen lookup is on this frame.
    pub(crate) fn uniform_screen_on(&self) -> bool {
        self.uniform.screen[3] != 0
    }

    /// Screen lookup region: first word, lookup width and height (zeros when off).
    pub(crate) fn screen_region(&self) -> [u32; 3] {
        let s = self.uniform.screen;
        if s[3] == 0 { [0; 3] } else { [s[0], s[1], s[2]] }
    }

    /// Size of the screen lookup (half resolution unless [`GiLookupResolution`] says otherwise).
    pub(crate) fn screen_half(&self) -> (u32, u32) {
        (self.uniform.screen[1], self.uniform.screen[2])
    }

    /// Bakes still running on the task pool.
    pub(crate) fn pending_bakes(&self) -> usize {
        self.pending.len()
    }

    /// Replaces the mesh keys that skip GI proxies. Empty means every candidate is a proxy.
    ///
    /// A mesh can cast shadows and still be in this set ([`crate::MeshRenderer::gi_proxy`]
    /// false). [`ShadowCast::Off`] stays a separate exclusion.
    pub(crate) fn set_gi_proxy_off(&mut self, keys: impl IntoIterator<Item = u64>) {
        self.gi_proxy_off.clear();
        self.gi_proxy_off.extend(keys);
    }

    /// Whether `mesh_key` has a brick in the pool.
    pub(crate) fn brick_resident(&self, mesh_key: u64) -> bool {
        self.bricks.contains_key(&mesh_key)
    }

    /// Whether `mesh_key`'s brick is in this frame's probe instance list (the rays trace it).
    pub(crate) fn brick_traced(&self, mesh_key: u64) -> bool {
        let Some(slot) = self.bricks.get(&mesh_key) else {
            return false;
        };
        self.instances
            .iter()
            .any(|instance| instance.brick[0] == slot.offset)
    }

    /// Pool offset of `mesh_key`'s brick, when it is resident.
    #[cfg(test)]
    fn brick_offset(&self, mesh_key: u64) -> Option<u32> {
        self.bricks.get(&mesh_key).map(|slot| slot.offset)
    }

    /// Times [`brick_value_count`] has run on this state.
    #[cfg(test)]
    fn fit_walks(&self) -> u64 {
        self.fit_walks
    }

    /// [`proxy_candidate`] for every mesh of the frame, with each mesh's object-space bounds
    /// walked once per `(mesh key, vertex count)` instead of twice a frame.
    #[cfg(test)]
    fn candidates(&mut self, meshes: &[FrameMesh<'_>]) -> Vec<bool> {
        self.fill_candidates(meshes);
        self.candidate_scratch.clone()
    }

    /// Writes this frame's candidate bits into `candidate_scratch`.
    fn fill_candidates(&mut self, meshes: &[FrameMesh<'_>]) {
        let frame = self.frame;
        self.candidate_scratch.clear();
        for mesh in meshes {
            // A counted mesh (no CPU vertices, ADR-7721): its upload's box and index count.
            let counted = self.counted_info(mesh);
            let bit = if let Some(info) = counted {
                proxy_eligible_with(mesh, info.indices)
                    && proxy_fits(
                        &mesh.model,
                        Vec3::from_array(info.lo),
                        Vec3::from_array(info.hi),
                        &self.settings,
                    )
            } else if !proxy_eligible(mesh) {
                false
            } else {
                let (lo, hi) = {
                    let extent = self
                        .extents
                        .entry((mesh.mesh_key, mesh.vertices.len()))
                        .or_insert_with(|| MeshExtent::of(mesh.vertices, frame));
                    extent.seen = frame;
                    (extent.lo, extent.hi)
                };
                proxy_fits(&mesh.model, lo, hi, &self.settings)
            };
            self.candidate_scratch.push(bit);
        }
        if frame.is_multiple_of(EXTENT_IDLE_FRAMES) {
            self.extents
                .retain(|_, e| frame.wrapping_sub(e.seen) < EXTENT_IDLE_FRAMES);
            self.brick_counts
                .retain(|_, c| frame.wrapping_sub(c.seen) < EXTENT_IDLE_FRAMES);
            let generation = self.pool_gen;
            self.unfit.retain(|_, memo| memo.generation == generation);
        }
    }

    /// Cached object-space bounds (tests).
    #[cfg(test)]
    pub(crate) fn cached_extents(&self) -> usize {
        self.extents.len()
    }
}

/// Inserts a free span and coalesces it with its neighbours. A span that ends at `pool_end`
/// shrinks the bump instead of staying on the free list.
fn insert_free(spans: &mut Vec<FreeSpan>, offset: u64, len: u64, pool_end: &mut u64) {
    if len == 0 {
        return;
    }
    let idx = spans
        .iter()
        .position(|span| span.offset > offset)
        .unwrap_or(spans.len());
    if idx > 0 && spans[idx - 1].offset + spans[idx - 1].len == offset {
        spans[idx - 1].len += len;
        if idx < spans.len() && spans[idx - 1].offset + spans[idx - 1].len == spans[idx].offset {
            spans[idx - 1].len += spans[idx].len;
            spans.remove(idx);
        }
        shrink_bump(spans, pool_end);
        return;
    }
    if idx < spans.len() && offset + len == spans[idx].offset {
        spans[idx].offset = offset;
        spans[idx].len += len;
        shrink_bump(spans, pool_end);
        return;
    }
    spans.insert(idx, FreeSpan { offset, len });
    shrink_bump(spans, pool_end);
}

fn shrink_bump(spans: &mut Vec<FreeSpan>, pool_end: &mut u64) {
    if spans
        .last()
        .is_some_and(|span| span.offset + span.len == *pool_end)
    {
        *pool_end = spans.pop().map_or(*pool_end, |span| span.offset);
    }
}

/// First-fit a span of `len` values, or bump `pool_end` when the tail has room.
fn take_span(spans: &mut Vec<FreeSpan>, pool_end: &mut u64, capacity: u64, len: u64) -> Option<u64> {
    if len == 0 {
        return None;
    }
    if let Some(index) = spans.iter().position(|span| span.len >= len) {
        let span = spans.remove(index);
        if span.len > len {
            spans.insert(
                index,
                FreeSpan {
                    offset: span.offset + len,
                    len: span.len - len,
                },
            );
        }
        return Some(span.offset);
    }
    if pool_end.saturating_add(len) <= capacity {
        let offset = *pool_end;
        *pool_end += len;
        return Some(offset);
    }
    None
}

/// Frames a cached [`MeshExtent`] survives without its mesh being drawn.
const EXTENT_IDLE_FRAMES: u32 = 600;

/// A mesh's object-space bounds, as [`proxy_candidate`] measures them.
#[derive(Clone, Copy, Debug)]
struct MeshExtent {
    lo: Vec3,
    hi: Vec3,
    /// Last frame (`DdgiState::frame`) that drew the mesh.
    seen: u32,
}

impl MeshExtent {
    fn of(vertices: &[crate::LabVertex], frame: u32) -> Self {
        let mut lo = Vec3::splat(f32::MAX);
        let mut hi = Vec3::splat(f32::MIN);
        for v in vertices {
            let p = Vec3::from_array(v.position);
            lo = lo.min(p);
            hi = hi.max(p);
        }
        Self { lo, hi, seen: frame }
    }

    /// [`Self::of`] for bare positions (an instance set's mesh).
    fn of_positions(positions: &[[f32; 3]], frame: u32) -> Self {
        let mut lo = Vec3::splat(f32::MAX);
        let mut hi = Vec3::splat(f32::MIN);
        for p in positions {
            let p = Vec3::from_array(*p);
            lo = lo.min(p);
            hi = hi.max(p);
        }
        Self { lo, hi, seen: frame }
    }
}

/// Largest [`DdgiSettings::update_divisor`] the probe shader can take: `ddgi_trace.wgsl` shifts it
/// left by up to 3 bits (cascade) and 15 more (inactive slowdown) before its `%`, and 2^13 << 18
/// still fits a `u32` (8192 frames is over two minutes at 60 fps).
pub const DDGI_MAX_UPDATE_DIVISOR: u32 = 1 << 13;

/// Updates a finest-cascade probe takes before a field whose traced scene stays unchanged stops
/// updating (ADR-5001). The freeze used to count 600 frames whatever the update divisor, 37 to 75
/// updates a probe on the shipped tiers, and held a sealed box's start-up transient (High 7.49 %
/// of the lit ground, ADR-4948 finding A). With every probe re-traced each frame the High layout
/// reads 0.51 % at 384 updates and 0.09 % at 768; 512 is past the `specs/27` § 2.2 bar of 0.5 %.
pub(crate) const DDGI_FREEZE_UPDATES: u32 = 512;

/// The freeze's floor in frames, the old fixed count: a layout that re-traces every probe each
/// frame keeps its 600 frames (ADR-5001).
pub(crate) const DDGI_FREEZE_MIN_FRAMES: u32 = 600;

/// Updates a new probe takes every frame before it joins its cascade's cadence (`ddgi_trace.wgsl`
/// `DDGI_YOUNG_UPDATES`, ADR-5506): what [`DdgiSettings::young_budget`] meters.
pub(crate) const DDGI_YOUNG_UPDATES: u32 = 16;

/// Words of the young-probe budget's buffer (`ddgi_trace.wgsl` `DdgiYoung`): the view-projection matrix, the
/// eye, the parameters, the passes' state and a 256-bin histogram.
pub(crate) const YOUNG_WORDS: u64 = 16 + 4 + 4 + 4 + 256;
/// Of those, the words the CPU writes each frame (the matrix, the eye and the parameters); the rest the
/// passes write, from zero each frame.
pub(crate) const YOUNG_PARAM_WORDS: u64 = 16 + 4 + 4;

/// The CPU-written head of the young-probe budget's buffer (`DdgiYoung` up to `state`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct YoungParamsGpu {
    /// The frame's view-projection matrix, columns.
    pub(crate) view_proj: [[f32; 4]; 4],
    /// xyz = the eye, w = 1 when `view_proj` holds the frame's view.
    pub(crate) eye: [f32; 4],
    /// x = the budget, y = f32 bits of the finest spacing.
    pub(crate) params: [u32; 4],
}

impl YoungParamsGpu {
    /// The parameters for `settings` at `eye` with the frame's `view_proj` (`None`: every probe counts as in
    /// view). All zero when the budget is off, so a moving camera rewrites nothing.
    pub(crate) fn of(settings: &DdgiSettings, eye: [f32; 3], view_proj: Option<Mat4>) -> Self {
        if settings.young_budget == 0 {
            return Self::default();
        }
        let view = view_proj.filter(Mat4::is_finite);
        let eye = eye.map(|v| if v.is_finite() { v } else { 0.0 });
        Self {
            view_proj: view.unwrap_or(Mat4::IDENTITY).to_cols_array_2d(),
            eye: [eye[0], eye[1], eye[2], if view.is_some() { 1.0 } else { 0.0 }],
            params: [
                settings.young_budget,
                settings.clone().clamped().spacing.to_bits(),
                0,
                0,
            ],
        }
    }
}

/// `ctrl.z` of the probe uniform: [`DdgiSettings::update_divisor`] in `1..=`
/// [`DDGI_MAX_UPDATE_DIVISOR`], so the shader's shifted divisor never wraps (to a short interval,
/// or to 0, a modulo by zero; audit v034 WGSL L1).
fn packed_update_divisor(s: &DdgiSettings) -> u32 {
    s.update_divisor.clamp(1, DDGI_MAX_UPDATE_DIVISOR)
}

/// Whether `mesh` gets a brick and is traced: rigid, opaque, shadow-casting, at least
/// `settings.min_proxy_radius` in bounding radius and fine enough that its brick voxel stays
/// within `settings.max_proxy_voxel`.
///
/// The frame path does not call this. It uses [`DdgiState::candidates`] (one bounds walk
/// per mesh) and [`DdgiState::proxy_bit`], which also applies [`DdgiState::gi_proxy_off`].
/// That opt-out is independent of [`ShadowCast`]: a mesh can cast shadows and still skip
/// the pool. [`ShadowCast::Off`] stays excluded here, so a sky dome does not become a proxy.
#[cfg(test)]
fn proxy_candidate(mesh: &FrameMesh<'_>, settings: &DdgiSettings) -> bool {
    if !proxy_eligible(mesh) {
        return false;
    }
    let extent = MeshExtent::of(mesh.vertices, 0);
    proxy_fits(&mesh.model, extent.lo, extent.hi, settings)
}

/// The material and shape half of [`proxy_candidate`] (no vertex walk).
fn proxy_eligible(mesh: &FrameMesh<'_>) -> bool {
    proxy_eligible_with(mesh, mesh.indices.len())
}

/// [`proxy_eligible`] for a mesh of `indices` indices (a counted mesh's are on the GPU,
/// ADR-7721).
fn proxy_eligible_with(mesh: &FrameMesh<'_>, indices: usize) -> bool {
    !(mesh.mesh_key == 0
        || !mesh.joints.is_empty()
        || mesh.material.unlit
        || crate::hair::effective_alpha_mode(mesh.material) == crate::lab_frame::ALPHA_BLEND
        || mesh.shadow == ShadowCast::Off
        || indices < 3)
}

/// The size half of [`proxy_candidate`], given the mesh's object-space bounds `lo`..`hi`.
fn proxy_fits(model: &[[f32; 4]; 4], lo: Vec3, hi: Vec3, settings: &DdgiSettings) -> bool {
    // Object-space radius × the largest axis scale.
    let m = Mat4::from_cols_array_2d(model);
    let scale = m
        .x_axis
        .truncate()
        .length()
        .max(m.y_axis.truncate().length())
        .max(m.z_axis.truncate().length());
    proxy_fits_scaled(scale, lo, hi, settings)
}

/// [`proxy_fits`] for a mesh drawn at `scale` (its model's longest axis).
fn proxy_fits_scaled(scale: f32, lo: Vec3, hi: Vec3, settings: &DdgiSettings) -> bool {
    let extent = (hi - lo) * scale;
    // A plate (a floor, a deck, a studio ground) is exact in its brick however coarse the
    // voxels along its plane: the distance across it is resolved by its own thin axis
    // (`ddgi_bake`'s anisotropic voxels) and the coarse axes run along a flat surface. Only
    // relief needs fine voxels along the plane, so the voxel limit applies to meshes thicker
    // than `max_proxy_voxel` / 8.
    let plate = extent.min_element() <= settings.max_proxy_voxel / 8.0;
    extent.length() * 0.5 >= settings.min_proxy_radius
        && (plate
            || extent.max_element() / crate::ddgi_bake::MAX_BRICK_DIM as f32 <= settings.max_proxy_voxel)
}

fn transform_aabb(m: &Mat4, lo: Vec3, hi: Vec3) -> (Vec3, Vec3) {
    let mut wmin = Vec3::splat(f32::MAX);
    let mut wmax = Vec3::splat(f32::MIN);
    for i in 0..8 {
        let p = Vec3::new(
            if i & 1 == 0 { lo.x } else { hi.x },
            if i & 2 == 0 { lo.y } else { hi.y },
            if i & 4 == 0 { lo.z } else { hi.z },
        );
        let w = m.transform_point3(p);
        wmin = wmin.min(w);
        wmax = wmax.max(w);
    }
    (wmin, wmax)
}

/// One texel of the screen lookup: irradiance / π (the frame's photometric units) toward the
/// six ambient-cube directions (+x, −x, +y, −y, +z, −z), and probe coverage (0 on sky texels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DdgiScreenTexel {
    /// Irradiance / π toward +x, −x, +y, −y, +z, −z.
    pub e: [[f32; 3]; 6],
    /// Probe coverage.
    pub coverage: f32,
}

impl DdgiScreenTexel {
    /// Irradiance / π for unit normal `n`, as the shaders rebuild it:
    /// n.x² e(±x) + n.y² e(±y) + n.z² e(±z).
    #[must_use]
    pub fn at(&self, n: [f32; 3]) -> [f32; 3] {
        let mut out = [0.0f32; 3];
        let total = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).max(1e-6);
        for (axis, &v) in n.iter().enumerate() {
            let e = self.e[2 * axis + usize::from(v < 0.0)];
            for c in 0..3 {
                out[c] += e[c] * v * v / total;
            }
        }
        out
    }
}

/// Resolution of the screen lookup that probe GI (the GPU probes or a baked volume) reaches the
/// forward and terrain passes through. Each lookup texel stores the irradiance at the surface
/// under the top-left pixel of its block; a fragment rebuilds its own from the 2 × 2 nearest
/// texels, weighted bilinearly and by surface distance, for its own normal
/// (`ddgi_screen_irradiance`). `Half` is what every preset uses. `Full` and `Quarter` are
/// measurement settings (ADR-1033: the reference half resolution is measured against, and the
/// next step down, rejected as a quality setting).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GiLookupResolution {
    /// One texel per pixel. The probe region holds 1920 × 1080 texels, so a larger frame's
    /// probe lookup stays at `Half`; a baked volume's buffer grows up to the device's
    /// storage-binding limit.
    Full,
    /// One texel per 2 × 2 pixels (the default).
    #[default]
    Half,
    /// One texel per 4 × 4 pixels.
    Quarter,
}

impl GiLookupResolution {
    /// Pixels per lookup texel along each axis: 1, 2 or 4.
    #[must_use]
    pub const fn step(self) -> u32 {
        match self {
            Self::Full => 1,
            Self::Half => 2,
            Self::Quarter => 4,
        }
    }

    /// The resolution whose [`Self::step`] is `step`, if any.
    #[must_use]
    pub const fn from_step(step: u32) -> Option<Self> {
        match step {
            1 => Some(Self::Full),
            2 => Some(Self::Half),
            4 => Some(Self::Quarter),
            _ => None,
        }
    }

    /// Lookup width and height for a `width` × `height` frame.
    #[must_use]
    pub const fn lookup_size(self, width: u32, height: u32) -> (u32, u32) {
        let step = self.step();
        (width.div_ceil(step), height.div_ceil(step))
    }
}

/// The lookup a `full` frame gets at `wanted`: (width, height, step). A lookup that `fits`
/// refuses falls back to the half-resolution default; `None` when that does not fit either (or
/// the frame is empty).
pub(crate) fn fit_lookup(
    full: (u32, u32),
    wanted: GiLookupResolution,
    fits: impl Fn(u32, u32) -> bool,
) -> Option<(u32, u32, u32)> {
    [wanted, GiLookupResolution::Half].into_iter().find_map(|r| {
        let (w, h) = r.lookup_size(full.0, full.1);
        (w > 0 && h > 0 && fits(w, h)).then_some((w, h, r.step()))
    })
}

/// Decodes the shader's RGB9E5 word (`ddgi_unpack_rgb9e5`).
#[must_use]
pub(crate) fn unpack_rgb9e5(v: u32) -> [f32; 3] {
    let scale = 2f32.powi((v >> 27) as i32 - 24);
    [v & 511, (v >> 9) & 511, (v >> 18) & 511].map(|m| m as f32 * scale)
}

/// Contract rows of the DDGI structs (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    use std::mem::{offset_of, size_of};
    let cascades = offset_of!(DdgiUniformGpu, cascades);
    let cascade = size_of::<CascadeGpu>();
    let u = |name, field_offset| (name, field_offset);
    vec![
        crate::ContractRow::new(
            "",
            "DdgiUniform",
            size_of::<DdgiUniformGpu>(),
            vec![
                u("cas_a", cascades),
                u("cas_b", cascades + cascade),
                u("cas_c", cascades + 2 * cascade),
                u("cas_d", cascades + 3 * cascade),
                u("dims", offset_of!(DdgiUniformGpu, dims)),
                u("params", offset_of!(DdgiUniformGpu, params)),
                u("ctrl", offset_of!(DdgiUniformGpu, ctrl)),
                u("rot_x", offset_of!(DdgiUniformGpu, rot)),
                u("rot_y", offset_of!(DdgiUniformGpu, rot) + 16),
                u("rot_z", offset_of!(DdgiUniformGpu, rot) + 32),
                u("to_sun", offset_of!(DdgiUniformGpu, to_sun)),
                u("sun", offset_of!(DdgiUniformGpu, sun)),
                u("sky", offset_of!(DdgiUniformGpu, sky)),
                u("flat_sky", offset_of!(DdgiUniformGpu, flat_sky)),
                u("inst_offset", offset_of!(DdgiUniformGpu, inst_offset)),
                u("inst_count", offset_of!(DdgiUniformGpu, inst_count)),
                u("words", offset_of!(DdgiUniformGpu, words)),
                u("screen", offset_of!(DdgiUniformGpu, screen)),
                u("emitters", offset_of!(DdgiUniformGpu, emitters)),
                u("to_fill", offset_of!(DdgiUniformGpu, to_fill)),
                u("fill", offset_of!(DdgiUniformGpu, fill)),
                u("extra_dir", offset_of!(DdgiUniformGpu, extra_dir)),
            ],
        ),
        crate::contract_row!(
            "ddgi_blend.wgsl",
            "DdgiEmitter",
            EmitterGpu {
                pos_radius,
                power,
                normal_flat
            }
        ),
        crate::contract_row!(
            "ddgi_trace.wgsl",
            "Heightfield",
            HeightfieldGpu {
                map,
                size,
                albedo,
                shadow
            }
        ),
        crate::ContractRow::new(
            "ddgi_trace.wgsl",
            "SdfInstance",
            size_of::<SdfInstanceGpu>(),
            vec![
                ("w2l0", offset_of!(SdfInstanceGpu, w2l)),
                ("w2l1", offset_of!(SdfInstanceGpu, w2l) + 16),
                ("w2l2", offset_of!(SdfInstanceGpu, w2l) + 32),
                ("aabb_min", offset_of!(SdfInstanceGpu, aabb_min)),
                ("aabb_max", offset_of!(SdfInstanceGpu, aabb_max)),
                ("brick_origin", offset_of!(SdfInstanceGpu, brick_origin)),
                ("brick_voxel", offset_of!(SdfInstanceGpu, brick_voxel)),
                ("brick", offset_of!(SdfInstanceGpu, brick)),
                ("albedo", offset_of!(SdfInstanceGpu, albedo)),
                ("emissive", offset_of!(SdfInstanceGpu, emissive)),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit v034 WGSL L1: `ddgi_trace.wgsl` shifts `ctrl.z` left by up to 3 (cascade) plus 15
    /// (inactive slowdown) bits and then takes `% divisor`. No setting may make that wrap: a
    /// wrapped divisor is a much shorter interval, and one that wraps to 0 is a modulo by zero.
    #[test]
    fn the_update_divisor_never_wraps_in_the_shader() {
        for divisor in [0, 1, 7, 8_192, 16_384, 1 << 20, u32::MAX] {
            let s = DdgiSettings {
                update_divisor: divisor,
                inactive_slowdown: u32::MAX,
                ..DdgiSettings::default()
            };
            let packed = packed_update_divisor(&s);
            assert!(packed >= 1, "{divisor}: {packed}");
            let widest = u64::from(packed) << (3 + 15);
            assert!(
                widest <= u64::from(u32::MAX),
                "{divisor}: {packed} << 18 wraps in a u32"
            );
        }
        let s = DdgiSettings::default();
        assert_eq!(
            packed_update_divisor(&s),
            s.update_divisor,
            "the default is unchanged"
        );
    }

    /// ADR-5001 (ADR-4948 finding A): the probe update freezes after a count of updates a
    /// finest probe takes, not a count of frames, so a tier that re-traces a settled probe every
    /// 8 or 16 frames converges before it stops; a layout that re-traces every probe each frame
    /// keeps the old 600 frames; no divisor overflows the count.
    #[test]
    fn the_probe_freeze_counts_updates_a_probe() {
        for tier in crate::DynamicGiTier::ALL {
            let s = tier.settings();
            let updates = s.freeze_frames() / s.update_divisor;
            assert!(
                updates >= DDGI_FREEZE_UPDATES,
                "{}: {} frames at divisor {} are {updates} updates",
                tier.name(),
                s.freeze_frames(),
                s.update_divisor
            );
        }
        assert_eq!(crate::DynamicGiTier::High.settings().freeze_frames(), 4096);
        assert_eq!(crate::DynamicGiTier::Low.settings().freeze_frames(), 8192);
        let every_frame = DdgiSettings {
            update_divisor: 1,
            ..DdgiSettings::default()
        };
        assert_eq!(every_frame.freeze_frames(), DDGI_FREEZE_MIN_FRAMES);
        for divisor in [0, 4, DDGI_MAX_UPDATE_DIVISOR, u32::MAX] {
            let s = DdgiSettings {
                update_divisor: divisor,
                ..DdgiSettings::default()
            };
            assert!(s.freeze_frames() >= DDGI_FREEZE_MIN_FRAMES, "{divisor}");
            assert_eq!(
                s.freeze_frames(),
                (DDGI_FREEZE_UPDATES * packed_update_divisor(&s)).max(DDGI_FREEZE_MIN_FRAMES),
                "{divisor}"
            );
        }
    }

    /// ADR-7870: with a young-probe budget a restart takes up to `⌊16 · probes / budget⌋ + 16` frames, and the
    /// freeze never comes before that; without one (every tier) the freeze is the count above, unchanged.
    #[test]
    fn the_freeze_waits_for_a_field_that_restarted_under_a_young_budget() {
        let high = crate::DynamicGiTier::High.settings();
        assert_eq!(high.probe_count(), 19_200);
        assert_eq!(high.young_settle_frames(), DDGI_YOUNG_UPDATES);
        let budgeted = |young_budget: u32| DdgiSettings {
            young_budget,
            ..high.clone()
        };
        assert_eq!(budgeted(4096).young_settle_frames(), 16 * 19_200 / 4096 + 16);
        assert_eq!(budgeted(19_200).young_settle_frames(), 32);
        assert_eq!(budgeted(u32::MAX).young_settle_frames(), 16);
        // Small budgets stretch the restart past the update freeze, which then waits for it.
        assert_eq!(budgeted(4096).freeze_frames(), high.freeze_frames());
        assert_eq!(budgeted(16).freeze_frames(), 19_200 + 16);
        assert_eq!(budgeted(1).freeze_frames(), 16 * 19_200 + 16);
        for tier in crate::DynamicGiTier::ALL {
            let s = tier.settings();
            assert_eq!(
                s.freeze_frames(),
                (DDGI_FREEZE_UPDATES * packed_update_divisor(&s)).max(DDGI_FREEZE_MIN_FRAMES),
                "{}",
                tier.name()
            );
        }
    }

    /// The young-probe budget's parameters are all zero while it is off (a moving camera queues no upload), and
    /// carry the budget, the eye, the view and the finest spacing when it is on.
    #[test]
    fn the_young_budget_parameters_are_zero_while_it_is_off() {
        let view = Mat4::from_translation(Vec3::new(1.0, -2.0, 3.0)) * Mat4::from_rotation_y(0.4);
        assert_eq!(
            YoungParamsGpu::of(&DdgiSettings::default(), [3.0, 2.0, 1.0], Some(view)),
            YoungParamsGpu::default()
        );
        let on = DdgiSettings {
            young_budget: 500,
            spacing: 1.5,
            ..DdgiSettings::default()
        };
        let p = YoungParamsGpu::of(&on, [3.0, 2.0, f32::NAN], Some(view));
        assert_eq!(p.params, [500, 1.5f32.to_bits(), 0, 0]);
        assert_eq!(p.eye, [3.0, 2.0, 0.0, 1.0]);
        assert_eq!(p.view_proj, view.to_cols_array_2d());
        let blind = YoungParamsGpu::of(&on, [0.0; 3], Some(Mat4::from_cols_array(&[f32::NAN; 16])));
        assert_eq!(blind.eye[3], 0.0, "a bad view counts every probe as in view");
        assert_eq!(size_of::<YoungParamsGpu>() as u64, YOUNG_PARAM_WORDS * 4);
    }

    #[test]
    fn clamped_settings_bound_the_probe_count() {
        let huge = DdgiSettings {
            dims: [4096, 1024, 1024],
            cascades: 1,
            ..DdgiSettings::default()
        };
        let c = huge.clamped();
        assert!(c.probe_count() <= DdgiSettings::MAX_PROBES, "{}", c.probe_count());
        assert!(c.dims.iter().all(|&d| d >= 1));
        let many = DdgiSettings {
            cascades: 99,
            ..DdgiSettings::default()
        };
        assert_eq!(many.clamped().cascades, MAX_CASCADES as u32);
        assert_eq!(DdgiSettings::default().clamped().dims, [20, 10, 20]);
    }

    /// Audit fix-render4 finding 3: `spacing_ratio` of 0 makes cascade 1's spacing 0.
    /// `prepare` must return, and cascade 1 must sit on the positive fallback spacing.
    #[test]
    fn zero_spacing_ratio_prepare_uses_a_positive_fallback() {
        let eye = [1.0, 2.0, 3.0];
        let settings = DdgiSettings {
            cascades: 2,
            spacing: 2.0,
            spacing_ratio: 0.0,
            ..DdgiSettings::default()
        };
        let mut state = DdgiState::new(settings);
        state.prepare(eye, &flat_light(), &[], &[], false, true);
        let fallback = DdgiSettings {
            cascades: 2,
            spacing: 2.0,
            spacing_ratio: DdgiSettings::default().spacing_ratio,
            ..DdgiSettings::default()
        };
        assert!(fallback.spacing_of(1) > 0.0);
        for c in 0..2 {
            let lift = cascade_lift_cells(&fallback, c);
            let expected = cascade_base(eye, fallback.spacing_of(c), fallback.dims, lift);
            let base = state.uniform.cascade_base(c as usize);
            assert_eq!([base[0], base[1], base[2]], expected, "cascade {c}");
            assert!(state.uniform.cascades[c as usize].origin[3] > 0.0);
        }
    }

    /// Audit fix-render4 finding 3: a negative spacing inverts the cascade box. `prepare`
    /// must keep `lo <= hi` and a probe base inside the volume around the eye.
    #[test]
    fn negative_spacing_prepare_keeps_a_finite_volume_around_the_eye() {
        let eye = [1.0, 2.0, 3.0];
        let settings = DdgiSettings {
            cascades: 2,
            spacing: -2.0,
            ..DdgiSettings::default()
        };
        let mut state = DdgiState::new(settings);
        state.prepare(eye, &flat_light(), &[], &[], false, true);
        let fallback = DdgiSettings {
            cascades: 2,
            spacing: DdgiSettings::default().spacing,
            ..DdgiSettings::default()
        };
        let dims = state.uniform.dims();
        for c in 0..2u32 {
            let gpu = &state.uniform.cascades[c as usize];
            let spacing = gpu.origin[3];
            assert!(
                spacing.is_finite() && spacing > 0.0,
                "cascade {c} spacing {spacing}"
            );
            let lift = cascade_lift_cells(&fallback, c);
            let expected = cascade_base(eye, fallback.spacing_of(c), fallback.dims, lift);
            assert_eq!([gpu.base[0], gpu.base[1], gpu.base[2]], expected, "cascade {c}");
            for a in 0..3 {
                let lo = gpu.base[a] as f32 * spacing;
                let hi = (gpu.base[a] + dims[a] as i32 - 1) as f32 * spacing;
                assert!(lo <= hi, "cascade {c} axis {a}: {lo} > {hi}");
                let eye_cell = (eye[a] / spacing).floor() as i32;
                assert!(
                    gpu.base[a] <= eye_cell && eye_cell < gpu.base[a].saturating_add(dims[a] as i32),
                    "cascade {c} axis {a} base {} does not hold the eye cell {eye_cell}",
                    gpu.base[a]
                );
            }
        }
    }

    /// A non-finite camera eye overflowed `cascade_base` (`i32::MIN` minus the volume's
    /// half) and left `cascade_origin` non-finite, so `prepare` died or uploaded a NaN
    /// cascade. A bad eye axis falls back to 0. A finite eye past the `i32` cell grid
    /// saturates, and the origin stays finite.
    #[test]
    fn a_non_finite_eye_keeps_a_finite_cascade() {
        let settings = DdgiSettings::default();
        let dims = settings.dims;
        let spacing = settings.spacing;
        let lift = cascade_lift_cells(&settings, 0);
        let settled = cascade_base([0.0, 0.0, 0.0], spacing, dims, lift);
        let bad = [f32::NEG_INFINITY, f32::NAN, 1.0];
        let origin = cascade_origin(bad, spacing, dims, lift);
        assert!(origin.iter().all(|c| c.is_finite()), "{origin:?}");
        let base = cascade_base(bad, spacing, dims, lift);
        assert_eq!(base[0], settled[0], "a non-finite eye axis falls back to 0");
        assert_eq!(base[1], settled[1], "a NaN eye axis falls back to 0");
        let far = [-1.0e10, 0.0, 0.0];
        let far_base = cascade_base(far, spacing, dims, lift);
        let far_origin = cascade_origin(far, spacing, dims, lift);
        assert!(
            far_origin.iter().all(|c| c.is_finite()),
            "{far_base:?} {far_origin:?}"
        );
        assert!(far_base[0] < 0, "the cell grid wrapped: {far_base:?}");
        let mut state = DdgiState::new(settings);
        state.prepare(
            [f32::NEG_INFINITY, f32::NAN, -1.0e10],
            &flat_light(),
            &[],
            &[],
            false,
            true,
        );
        for (c, gpu) in state.uniform.cascades.iter().enumerate() {
            assert!(
                gpu.origin.iter().all(|v| v.is_finite()) && gpu.origin[3] > 0.0,
                "cascade {c}: {gpu:?}"
            );
        }
    }

    #[test]
    fn cached_proxy_bounds_match_the_vertex_walk() {
        // A rock (1 m), a pebble (below `min_proxy_radius`) and a plate, each scaled and moved:
        // the cached test must answer as the per-frame walk did, and walk each mesh once.
        static MATERIAL: crate::LabMaterial = crate::LabMaterial::EMPTY;
        let settings = DdgiSettings::default();
        let mut state = DdgiState::new(settings.clone());
        let shapes: Vec<(Vec<crate::LabVertex>, Vec<u32>)> =
            [[0.5, 0.5, 0.5], [0.01, 0.01, 0.01], [8.0, 0.01, 8.0]]
                .into_iter()
                .map(crate::box_mesh)
                .collect();
        let meshes: Vec<FrameMesh<'_>> = shapes
            .iter()
            .enumerate()
            .flat_map(|(k, (vertices, indices))| {
                [1.0f32, 3.0].into_iter().map(move |scale| FrameMesh {
                    is_static: false,
                    mesh_key: k as u64 + 1,
                    material_key: 1,
                    object_key: k as u64 * 2 + scale as u64,
                    vertices,
                    indices,
                    material: &MATERIAL,
                    model: (Mat4::from_translation(Vec3::new(scale, 0.0, 0.0))
                        * Mat4::from_scale(Vec3::splat(scale)))
                    .to_cols_array_2d(),
                    joints: &[],
                    receiver_layer: 0,
                    shadow: ShadowCast::All,
                    uv1: &[],
                    lod_dither: 0.0,
                })
            })
            .collect();
        let want: Vec<bool> = meshes.iter().map(|m| proxy_candidate(m, &settings)).collect();
        assert!(want.iter().any(|&c| c) && want.iter().any(|&c| !c), "{want:?}");
        for _ in 0..3 {
            state.frame += 1;
            assert_eq!(state.candidates(&meshes), want);
        }
        assert_eq!(state.cached_extents(), shapes.len(), "one walk per mesh key");
    }

    #[test]
    fn gpu_layouts_match_the_shaders() {
        // 17 rows, + the six `extra_dir` rows of the frame's extra directional lights (ADR-7254).
        assert_eq!(size_of::<DdgiUniformGpu>(), 4 * 32 + 16 * 23);
        assert_eq!(size_of::<EmitterGpu>(), 48);
        assert_eq!(size_of::<SdfInstanceGpu>(), 160);
        assert_eq!(size_of::<HeightfieldGpu>(), 64);
    }

    #[test]
    fn cascades_scroll_by_whole_cells() {
        let dims = [24, 12, 24];
        let lift = eye_lift_cells(dims[1], 2.0, 0, FINEST_HEADROOM_M);
        let a = cascade_base([10.0, 5.0, -3.0], 2.0, dims, lift);
        let b = cascade_base([10.9, 5.0, -3.0], 2.0, dims, lift);
        let c = cascade_base([12.1, 5.0, -3.0], 2.0, dims, lift);
        assert_eq!(a, b, "inside one cell the volume holds still");
        assert_eq!(c[0], a[0] + 1);
        // The eye sits inside the volume, a little above its middle.
        let eye_cell = (5.0f32 / 2.0).floor() as i32;
        assert!(eye_cell > a[1] && eye_cell < a[1] + 12);
        assert!(eye_cell - a[1] >= 6 && a[1] + 11 - eye_cell >= 4);
    }

    /// `ddgi_sample.wgsl` `ddgi_inset`: cells inside the snapped volume (`base`).
    fn snapped_inset(gpu: &CascadeGpu, dims: [u32; 3], p: [f32; 3]) -> f32 {
        let s = gpu.origin[3];
        (0..3)
            .map(|a| {
                let g = p[a] / s - gpu.base[a] as f32;
                g.min(dims[a] as f32 - 1.0 - g)
            })
            .fold(f32::MAX, f32::min)
    }

    /// `ddgi_sample.wgsl` `ddgi_blend_inset`: cells inside the blend window, measured from the
    /// unsnapped corner (`origin.xyz`) with its far side a cell short.
    fn blend_inset(gpu: &CascadeGpu, dims: [u32; 3], p: [f32; 3]) -> f32 {
        let s = gpu.origin[3];
        (0..3)
            .map(|a| {
                let g = p[a] / s - gpu.origin[a];
                g.min(dims[a] as f32 - 2.0 - g)
            })
            .fold(f32::MAX, f32::min)
    }

    /// `ddgi_sample.wgsl` `ddgi_blend_weight`: the share of its turn cascade `c` takes at a window
    /// inset of `inset` cells (the finest cascade of a layout that needs the room rule keeps a quarter
    /// cell of margin, ADR-5909).
    fn blend_weight(uniform: &DdgiUniformGpu, c: usize, inset: f32) -> f32 {
        let room = c == 0 && uniform.ctrl[3] & CTRL_RAY_KEEPS_CASCADE != 0;
        let margin = if room {
            ROOM_MARGIN_CELLS
        } else {
            PLAIN_MARGIN_CELLS
        };
        (inset - margin).clamp(0.0, 1.0)
    }

    /// `ddgi_irradiance`'s cascade mix at `p` when cascade `c` reads a flat `e[c]` (every probe
    /// traced), with `inset` measuring the blend window: the shader's `blend_inset`, or
    /// `snapped_inset` as the lookup blended before ADR-424.
    fn cascade_mix(
        uniform: &DdgiUniformGpu,
        e: &[f32],
        p: [f32; 3],
        inset: fn(&CascadeGpu, [u32; 3], [f32; 3]) -> f32,
    ) -> f32 {
        let dims = [uniform.dims[0], uniform.dims[1], uniform.dims[2]];
        let (mut sum, mut left, mut covered) = (0.0f32, 1.0f32, 0.0f32);
        let mut edge = None;
        for (c, gpu) in uniform.cascades.iter().enumerate().take(uniform.dims[3] as usize) {
            if left <= 1e-3 {
                break;
            }
            if edge.is_none() && snapped_inset(gpu, dims, p) >= 0.0 {
                edge = Some(c);
            }
            let w = blend_weight(uniform, c, inset(gpu, dims, p));
            if w <= 0.0 {
                continue;
            }
            let take = w * left;
            sum += e[c] * take;
            covered += take;
            left -= take;
        }
        if covered <= 1e-5 {
            return edge.map_or(-1.0, |c| e[c]);
        }
        sum / covered
    }

    #[test]
    fn the_unsnapped_corner_stays_within_a_cell_above_the_snapped_one() {
        let dims = DdgiSettings::default().dims;
        // Cell boundaries and the floats just below them, far from the origin and near it.
        let below_one = 1.0 - f32::EPSILON / 2.0;
        let mut eyes = vec![
            [0.0, 0.0, 0.0],
            [2.0 - f32::EPSILON, 2.0, -1e-7],
            [below_one, -below_one, 5_000.25],
            [-4_999.9, 731.3, -2_000.0],
        ];
        for i in 0..64u32 {
            eyes.push([
                hash01(i * 3) * 2_000.0 - 1_000.0,
                hash01(i * 3 + 1) * 400.0 - 100.0,
                hash01(i * 3 + 2) * 2_000.0 - 1_000.0,
            ]);
        }
        for eye in eyes {
            for (c, spacing) in [2.0, 6.0, 18.0, 54.0].into_iter().enumerate() {
                let lift = eye_lift_cells(dims[1], spacing, c as u32, FINEST_HEADROOM_M);
                let base = cascade_base(eye, spacing, dims, lift);
                let origin = cascade_origin(eye, spacing, dims, lift);
                for (&o, &b) in origin.iter().zip(&base) {
                    let f = o - b as f32;
                    assert!(
                        (0.0..=1.0).contains(&f),
                        "eye {eye:?} spacing {spacing}: corner {origin:?} vs base {base:?}"
                    );
                }
                // Wherever the blend window gives a cascade any weight, the snapped volume
                // holds the probes the lookup reads (the shader's 0.75-cell margin keeps).
                let gpu = CascadeGpu {
                    origin: [origin[0], origin[1], origin[2], spacing],
                    base: [base[0], base[1], base[2], 0],
                };
                for k in 0..32u32 {
                    let p: [f32; 3] = std::array::from_fn(|a| {
                        (base[a] as f32 + hash01(k * 7 + a as u32) * dims[a] as f32) * spacing
                    });
                    assert!(
                        snapped_inset(&gpu, dims, p) >= blend_inset(&gpu, dims, p) - 1e-3,
                        "eye {eye:?} spacing {spacing} point {p:?}"
                    );
                }
            }
        }
        // A move inside one cell keeps the snapped corner and moves the unsnapped one with it.
        let lift = eye_lift_cells(dims[1], 2.0, 0, FINEST_HEADROOM_M);
        let a = cascade_origin([10.0, 5.0, -3.0], 2.0, dims, lift);
        let b = cascade_origin([10.9, 5.0, -3.0], 2.0, dims, lift);
        assert!((b[0] - a[0] - 0.45).abs() < 1e-4, "{a:?} {b:?}");
        assert_eq!(
            cascade_base([10.0, 5.0, -3.0], 2.0, dims, lift),
            cascade_base([10.9, 5.0, -3.0], 2.0, dims, lift)
        );
    }

    /// `ddgi_irradiance_traced_vis`'s mix between the finest cascade holding `p` and a coarser one that
    /// sees only `vis` of what the finest sees, each reading a flat light, with the fade in `fade`.
    fn vis_mix(
        uniform: &DdgiUniformGpu,
        e: &[f32],
        vis: f32,
        p: [f32; 3],
        fade: fn(&CascadeGpu, [u32; 3], [f32; 3]) -> f32,
    ) -> f32 {
        let dims = [uniform.dims[0], uniform.dims[1], uniform.dims[2]];
        let (mut sum, mut left, mut covered, mut to_fine) = (0.0f32, 1.0f32, 0.0f32, 0.0f32);
        let mut edge: Option<usize> = None;
        let mut edge_fade = 1.0;
        for (c, gpu) in uniform.cascades.iter().enumerate().take(uniform.dims[3] as usize) {
            if left <= 1e-3 {
                break;
            }
            if edge.is_none() && snapped_inset(gpu, dims, p) >= 0.0 {
                edge = Some(c);
                edge_fade = fade(gpu, dims, p);
            }
            let w = blend_weight(uniform, c, blend_inset(gpu, dims, p));
            if w <= 0.0 {
                continue;
            }
            let take = w * left;
            let coarser = edge.is_some_and(|f| c > f);
            let fine = edge.map_or(e[c], |f| e[f]);
            let (keep, light) = if coarser {
                let keep = (vis / 0.75).clamp(0.0, 1.0);
                let capped = e[c].min(fine * 1.25);
                (1.0 + (keep - 1.0) * edge_fade, e[c] + (capped - e[c]) * edge_fade)
            } else {
                (1.0, e[c])
            };
            sum += light * take * keep;
            to_fine += take * (1.0 - keep);
            covered += take;
            left -= take;
        }
        let fine = edge.map_or(0.0, |f| e[f]);
        if covered <= 1e-5 {
            fine
        } else {
            (sum + fine * to_fine) / covered
        }
    }

    /// ADR-2605: a coarser cascade's cap and keep stop before the finest cascade's edge along the
    /// unsnapped window, so the seam does not jump a whole cell when a volume scrolls (the pop ADR-424
    /// removed from the cascade blend). Applied up to the snapped edge they did.
    #[test]
    fn the_visibility_blend_does_not_pop_when_the_volumes_scroll() {
        let wgsl = crate::ddgi_gpu::ddgi_shading_wgsl();
        assert!(wgsl.contains("fn ddgi_vis_fade("), "the shader fades the blend");
        let settings = DdgiSettings {
            cascades: 3,
            spacing: 2.0,
            ..DdgiSettings::default()
        };
        let mut state = DdgiState::new(settings);
        // The coarser cascade reads twice the finest and sees half of what it sees: the room's case.
        let e = [1.0f32, 2.0, 2.0];
        let points: Vec<[f32; 3]> = (0..480u32)
            .map(|i| {
                [
                    -75.0 + i as f32 * 0.365,
                    0.5,
                    if i.is_multiple_of(3) { 7.3 } else { 0.0 },
                ]
            })
            .collect();
        let mut faded = Vec::new();
        let mut hard = Vec::new();
        for step in 0..=240u32 {
            let eye = [step as f32 * 0.1, 2.0, 0.0];
            state.prepare(eye, &flat_light(), &[], &[], false, true);
            faded.push(
                points
                    .iter()
                    .map(|&p| vis_mix(&state.uniform, &e, 0.5, p, vis_fade))
                    .collect::<Vec<f32>>(),
            );
            hard.push(
                points
                    .iter()
                    .map(|&p| vis_mix(&state.uniform, &e, 0.5, p, |_, _, _| 1.0))
                    .collect::<Vec<f32>>(),
            );
        }
        let worst = |run: &[Vec<f32>]| {
            run.windows(2)
                .flat_map(|w| w[0].iter().zip(&w[1]).map(|(a, b)| (a - b).abs()))
                .fold(0.0f32, f32::max)
        };
        let (faded, hard) = (worst(&faded), worst(&hard));
        eprintln!("worst step of the visibility blend: faded {faded}, up to the snapped edge {hard}");
        assert!(hard > 0.5, "without the fade the seam jumps: {hard}");
        // A 0.1 m step across a ramp a quarter of a cell wide (2 m cells: 0.5 m) moves at most a fifth
        // of the difference, plus the plain blend's own step.
        assert!(faded < 0.35, "the seam moves with the eye: {faded}");
    }

    /// `ddgi_vis_fade`: full above a quarter cell inside the unsnapped window, 0 at its end, and the
    /// snapped inset is never below the unsnapped one, so it is 0 by the time the edge changes.
    fn vis_fade(gpu: &CascadeGpu, dims: [u32; 3], p: [f32; 3]) -> f32 {
        (blend_inset(gpu, dims, p) / 0.25).clamp(0.0, 1.0)
    }

    /// ADR-424: the camera walks 24 m in 0.1 m steps and the lookup's cascade mix is read at
    /// world points across the cascade 0 → 1 and 1 → 2 blend bands, with each cascade a
    /// different flat light. Blending from the snapped corner (before ADR-424) jumped by up
    /// to the whole cascade difference whenever a volume scrolled a cell; the unsnapped window
    /// changes by at most the eye's step over the cell size.
    #[test]
    fn the_cascade_blend_does_not_pop_when_the_volumes_scroll() {
        let settings = DdgiSettings {
            cascades: 3,
            ..DdgiSettings::default()
        };
        let mut state = DdgiState::new(settings);
        let e = [1.0f32, 0.6, 0.3];
        // Inside the last cascade's blend window all along the walk (its own outer edge is
        // not a blend into a next cascade).
        let points: Vec<[f32; 3]> = (0..480u32)
            .map(|i| {
                [
                    -75.0 + i as f32 * 0.365,
                    0.5,
                    if i.is_multiple_of(3) { 7.3 } else { 0.0 },
                ]
            })
            .collect();
        let mut unsnapped = Vec::new();
        let mut snapped = Vec::new();
        for step in 0..=240u32 {
            let eye = [step as f32 * 0.1, 2.0, 0.0];
            state.prepare(eye, &flat_light(), &[], &[], false, true);
            unsnapped.push(
                points
                    .iter()
                    .map(|&p| cascade_mix(&state.uniform, &e, p, blend_inset))
                    .collect::<Vec<f32>>(),
            );
            snapped.push(
                points
                    .iter()
                    .map(|&p| cascade_mix(&state.uniform, &e, p, snapped_inset))
                    .collect::<Vec<f32>>(),
            );
        }
        let now =
            crate::temporal_stability::measure(&unsnapped, None, 0.0).unwrap_or_else(|err| panic!("{err}"));
        let before =
            crate::temporal_stability::measure(&snapped, None, 0.0).unwrap_or_else(|err| panic!("{err}"));
        // The walk does reach the bands: the light at the points changes along it.
        assert!(now.drift > 0.01, "the points never left cascade 0: {now:?}");
        // Cascade 0 (2 m cells) moves the window 0.05 cell a step: |Δ| ≤ 0.05 · (1.0 − 0.3).
        assert!(now.step_max <= 0.04, "the cascade blend pops: {now:?}");
        assert!(
            before.step_max >= 0.2,
            "the snapped blend should pop here (the test lost its reach): {before:?}"
        );
        assert!(
            now.temporal_information < before.temporal_information,
            "{now:?} vs {before:?}"
        );
    }

    /// The coverage `ddgi_irradiance_basis` gives the screen lookup at `p` (as it stores it,
    /// clamped at 0), every probe traced. `full_shell`: past every blend window the lookup read
    /// the cascade it is in at full coverage (before ADR-424) instead of none.
    fn screen_coverage(
        uniform: &DdgiUniformGpu,
        p: [f32; 3],
        inset: fn(&CascadeGpu, [u32; 3], [f32; 3]) -> f32,
        full_shell: bool,
    ) -> f32 {
        let dims = [uniform.dims[0], uniform.dims[1], uniform.dims[2]];
        let (mut left, mut covered) = (1.0f32, 0.0f32);
        let mut inside = false;
        for (c, gpu) in uniform.cascades.iter().enumerate().take(uniform.dims[3] as usize) {
            if left <= 1e-3 {
                break;
            }
            inside |= snapped_inset(gpu, dims, p) >= 0.0;
            let w = blend_weight(uniform, c, inset(gpu, dims, p));
            let take = w * left;
            covered += take;
            left -= take;
        }
        if covered <= 1e-5 {
            return if inside && full_shell { 1.0 } else { 0.0 };
        }
        covered
    }

    /// ADR-424: past the last cascade's blend window the screen lookup's coverage stays at the
    /// 0 it faded to. Reading the cascade at full coverage there drew a ring of probe light
    /// beyond the fade, and its edge (the snapped volume's) jumped a cell with every scroll.
    #[test]
    fn the_last_cascade_fades_out_without_a_ring() {
        let settings = DdgiSettings {
            cascades: 2,
            dims: [12, 8, 12],
            spacing: 1.0,
            ..DdgiSettings::default()
        };
        let mut state = DdgiState::new(settings);
        // Cascade 1 (3 m cells) fades out 6.75–9.75 m ahead of the eye and 12.75–15.75 m
        // behind it along x, and its snapped volume ends a little past each; the points
        // straddle both ends all along the walk.
        let points: Vec<[f32; 3]> = (0..400u32).map(|i| [-30.0 + i as f32 * 0.17, 0.3, 0.0]).collect();
        let mut now = Vec::new();
        let mut before = Vec::new();
        for step in 0..=96u32 {
            let eye = [step as f32 * 0.25, 1.7, 0.0];
            state.prepare(eye, &flat_light(), &[], &[], false, true);
            now.push(
                points
                    .iter()
                    .map(|&p| screen_coverage(&state.uniform, p, blend_inset, false))
                    .collect::<Vec<f32>>(),
            );
            before.push(
                points
                    .iter()
                    .map(|&p| screen_coverage(&state.uniform, p, snapped_inset, true))
                    .collect::<Vec<f32>>(),
            );
        }
        let now = crate::temporal_stability::measure(&now, None, 0.0).unwrap_or_else(|err| panic!("{err}"));
        let before =
            crate::temporal_stability::measure(&before, None, 0.0).unwrap_or_else(|err| panic!("{err}"));
        assert!(now.drift > 0.01, "the points never crossed the edge: {now:?}");
        // 0.25 m steps over 3 m cells: the coverage moves at most 1/12 a frame.
        assert!(now.step_max <= 0.25 / 3.0 + 1e-3, "the coverage pops: {now:?}");
        assert!(
            before.step_max >= 0.9,
            "the old shell should pop here: {before:?}"
        );
    }

    /// The shading lookup measures the blend from the unsnapped corner (ADR-424), in both the
    /// per-pixel lookup and the screen lookup's ambient-cube gather, and in their visibility-blend
    /// variants (ADR-2605).
    #[test]
    fn the_shading_lookup_blends_from_the_unsnapped_corner() {
        let wgsl = crate::ddgi_gpu::ddgi_shading_wgsl();
        // The four lookups take their blend weight from one function (ADR-5909), which measures the
        // unsnapped window.
        assert_eq!(
            wgsl.matches("ddgi_blend_weight(c, p);").count(),
            4,
            "ddgi_irradiance_traced (`_plain`), ddgi_irradiance_basis and their visibility-blend \
             variants (`_vis`) blend over the unsnapped window"
        );
        assert_eq!(
            wgsl.matches("clamp(ddgi_blend_inset(c, p) - select(0.75, DDGI_ROOM_MARGIN, room), 0.0, 1.0)")
                .count(),
            1,
            "`ddgi_blend_weight` ramps over the unsnapped window"
        );
        assert!(wgsl.contains("const DDGI_ROOM_MARGIN: f32 = 0.25;"));
        assert!(
            wgsl.contains("let room = c == 0u && (ddgi.ctrl.w & DDGI_CTRL_ROOM_RULE) != 0u;")
                && wgsl.contains("const DDGI_CTRL_ROOM_RULE: u32 = 16384u;"),
            "the narrow margin is the finest cascade's on a layout that needs the room rule"
        );
        for f in [
            "fn ddgi_irradiance_traced_plain(",
            "fn ddgi_irradiance_traced_vis(",
            "fn ddgi_irradiance_basis(",
            "fn ddgi_irradiance_basis_vis(",
        ] {
            assert_eq!(wgsl.matches(f).count(), 1, "{f}");
        }
        assert!(
            !wgsl.contains("clamp(inset - 0.75"),
            "a blend from the snapped base pops on scroll"
        );
        assert!(wgsl.contains("let g = p / cas.origin.w - cas.origin.xyz;"));
        // Past every blend window the screen lookup's coverage stays 0 (no ring). The basis is
        // built in one constructor since the FXC rewrite (ADR-1583), the coverage is its second field.
        assert!(wgsl.contains("return DdgiBasis(e, select(0.0, -1.0, edge == 99u));"));
        // The CPU fills the corner it reads.
        let mut state = DdgiState::new(DdgiSettings::default());
        state.prepare([3.3, 2.0, -7.1], &flat_light(), &[], &[], false, true);
        let settings = DdgiSettings::default();
        let spacing = settings.spacing_of(1);
        let lift = cascade_lift_cells(&settings, 1);
        let origin = cascade_origin([3.3, 2.0, -7.1], spacing, settings.dims, lift);
        assert_eq!(
            state.uniform.cascades[1].origin,
            [origin[0], origin[1], origin[2], spacing]
        );
    }

    /// The share each cascade takes at `dy` metres above the eye (`ddgi_irradiance`'s loop, every
    /// probe traced).
    fn cascade_shares(settings: &DdgiSettings, dy: f32) -> Vec<f32> {
        let mut state = DdgiState::new(settings.clone());
        let eye = [0.3, 1.6, -0.4];
        state.prepare(eye, &flat_light(), &[], &[], false, true);
        let p = [eye[0], eye[1] + dy, eye[2]];
        let dims = [settings.dims[0], settings.dims[1], settings.dims[2]];
        let mut left = 1.0f32;
        state
            .uniform
            .cascades
            .iter()
            .enumerate()
            .take(settings.cascades as usize)
            .map(|(c, gpu)| {
                let take = blend_weight(&state.uniform, c, blend_inset(gpu, dims, p)) * left;
                left -= take;
                take
            })
            .collect()
    }

    /// ADR-1949: at 8 probes per axis the 60 % placement left every blend window's top under the
    /// eye, so a point at the eye's height was 45 % cascade 0 and the rest fell through to the 18
    /// and 54 m cascades, and a ceiling read those alone. On every tier the finest cascade
    /// now holds a room around the eye, from the floor to a ceiling 2 m over it.
    #[test]
    fn the_finest_cascade_holds_a_room_around_the_eye_on_every_tier() {
        for tier in crate::DynamicGiTier::ALL {
            let settings = tier.settings();
            for dy in [-1.6, -0.5, 0.0, 0.5, 1.0, 1.4, 1.9] {
                let shares = cascade_shares(&settings, dy);
                assert!(
                    shares[0] >= 0.999,
                    "{tier:?}: cascade 0 takes {shares:?} at {dy} m from the eye"
                );
            }
            let (above, below) = finest_window_m(&settings);
            assert!(above >= 2.0, "{tier:?}: {above} m above the eye");
            // ADR-5290: High and Ultra's 1 m probes in twelve layers hold 3 m under the eye and 3.5 m
            // over it (`specs/79` § 2), past a standing eye's 1.8 m floor; the 2 m tiers hold 3 m under.
            let floor = 3.0;
            if settings.spacing < 2.0 {
                assert!(above >= 3.5, "{tier:?}: {above} m above the eye");
            }
            assert!(below >= floor, "{tier:?}: {below} m below the eye");
        }
    }

    /// ADR-1949: the room is in metres, so a fine layout's short window goes to the floor first. At
    /// 1 m and 8 layers the window the volume is placed for is 2.5 m tall: a window placed a fixed cell
    /// over the eye left a standing eye's floor 20 % cascade 1. Since ADR-5909 the lookup holds half a
    /// cell more at each end of that window (the finest cascade's quarter-cell margin), so the numbers
    /// here are the placement's plus 0.5 m at 1 m probes and 0.25 m at 0.5 m probes.
    #[test]
    fn a_fine_layout_keeps_the_floor_under_a_standing_eye() {
        let fine = |dims_y: u32, spacing: f32| DdgiSettings {
            cascades: 4,
            dims: [12, dims_y, 12],
            spacing,
            ..DdgiSettings::default()
        };
        let close = |got: (f32, f32), want: (f32, f32)| {
            assert!(
                (got.0 - want.0).abs() < 1e-5 && (got.1 - want.1).abs() < 1e-5,
                "{got:?} vs {want:?}"
            );
        };
        close(finest_window_m(&fine(8, 1.0)), (1.2, 2.3));
        assert!(cascade_shares(&fine(8, 1.0), -1.7)[0] >= 0.999);
        // Tall enough for both.
        close(finest_window_m(&fine(10, 1.0)), (2.5, 3.0));
        // Too short for the floor alone: all of the placement's window goes under the eye.
        close(finest_window_m(&fine(8, 0.5)), (0.25, 1.5));
    }

    /// ADR-1949: a layout whose every cascade resolves a room does not leak (0.04 % at 1 and 3 m),
    /// so it keeps its place and its ray read, down to the bit. The room rule would only darken
    /// it under a fast camera (`tests/ddgi_temporal.rs`). Every tier needs the rule.
    #[test]
    fn a_layout_that_resolves_rooms_keeps_its_place_and_its_ray_read() {
        let fine = DdgiSettings {
            cascades: 2,
            dims: [12, 8, 12],
            spacing: 1.0,
            ..DdgiSettings::default()
        };
        let eye = [0.3, 1.7, -0.4];
        let mut state = DdgiState::new(fine.clone());
        state.prepare(eye, &flat_light(), &[], &[], false, true);
        assert_eq!(state.uniform.ctrl[3] & CTRL_RAY_KEEPS_CASCADE, 0);
        for c in 0..2 {
            let spacing = fine.spacing_of(c);
            // The placement before ADR-1949.
            let y = (eye[1] - 0.8 * spacing) / spacing;
            let base = state.uniform.cascades[c as usize].base;
            assert_eq!(base[1], y.floor() as i32 - 4, "cascade {c}");
        }

        for (name, settings, needs) in [
            (
                "a single wide cascade has nothing coarser",
                DdgiSettings {
                    cascades: 1,
                    spacing: 6.0,
                    ..fine.clone()
                },
                false,
            ),
            (
                "a 3 m coarsest cascade is the bound",
                DdgiSettings {
                    cascades: 2,
                    spacing: 1.5,
                    spacing_ratio: 2.0,
                    ..fine.clone()
                },
                false,
            ),
            (
                "past the bound",
                DdgiSettings {
                    cascades: 3,
                    ..fine.clone()
                },
                true,
            ),
        ] {
            assert_eq!(settings.needs_room_rule(), needs, "{name}");
            let mut state = DdgiState::new(settings);
            state.prepare(eye, &flat_light(), &[], &[], false, true);
            assert_eq!(
                state.uniform.ctrl[3] & CTRL_RAY_KEEPS_CASCADE != 0,
                needs,
                "{name}"
            );
        }
        for tier in crate::DynamicGiTier::ALL {
            assert!(tier.settings().needs_room_rule(), "{tier:?}");
        }
    }

    /// `finest_window_m` is what the lookup does: full weight just inside it, less just outside.
    #[test]
    fn the_reported_window_is_the_blend_windows_full_weight_span() {
        for tier in crate::DynamicGiTier::ALL {
            let settings = tier.settings();
            let (above, below) = finest_window_m(&settings);
            assert!(cascade_shares(&settings, above - 0.05)[0] >= 0.999, "{tier:?}");
            assert!(cascade_shares(&settings, above + 0.3)[0] < 0.999, "{tier:?}");
            assert!(cascade_shares(&settings, -below + 0.05)[0] >= 0.999, "{tier:?}");
            assert!(cascade_shares(&settings, -below - 0.3)[0] < 0.999, "{tier:?}");
        }
    }

    /// Every coarser cascade holds full weight at the eye, and on a 10-layer volume (Desktop High
    /// and Ultra) it stays where it was: the far ground under a high camera keeps its reach.
    #[test]
    fn coarser_cascades_reach_full_weight_at_the_eye_and_keep_their_reach() {
        for dims_y in [8u32, 10, 12, 16] {
            let lift = eye_lift_cells(dims_y, 2.0, 1, FINEST_HEADROOM_M);
            let eye = (dims_y / 2) as f32 + lift;
            assert!(
                dims_y as f32 - BLEND_TOP_CELLS - eye >= BLEND_MARGIN_CELLS,
                "{dims_y} layers: the window's top ends under the eye"
            );
        }
        assert!(
            (eye_lift_cells(10, 2.0, 1, FINEST_HEADROOM_M) - 1.0).abs() < 1e-6,
            "10 layers: 0.1 · dims as before"
        );
        assert!(
            (eye_lift_cells(12, 2.0, 2, FINEST_HEADROOM_M) - 1.2).abs() < 1e-6,
            "12 layers: 0.1 · dims as before"
        );
        // A short volume keeps the eye inside it and inside the window's bottom.
        for dims_y in [2u32, 3, 4, 5, 6, 7] {
            for c in [0, 1] {
                let eye = (dims_y / 2) as f32 + eye_lift_cells(dims_y, 2.0, c, FINEST_HEADROOM_M);
                assert!(
                    (0.0..=dims_y as f32).contains(&eye),
                    "{dims_y} layers, cascade {c}: {eye}"
                );
            }
        }
    }

    /// ADR-1949: a ray hit reads no cascade coarser than the probe that traced it, wherever that
    /// probe's cascade holds the hit, on a layout that needs the room rule. The screen lookup
    /// keeps the plain rule.
    #[test]
    fn a_ray_hit_reads_only_its_own_cascade_or_finer() {
        let trace = crate::ddgi_gpu::trace_source();
        assert_eq!(CTRL_RAY_KEEPS_CASCADE, 16384, "the bit `ddgi_trace.wgsl` tests");
        assert!(trace.contains("select(99u, c, (ddgi.ctrl.w & 16384u) != 0u)"));
        // The probes read without the shading's visibility blend (ADR-2605): `_plain` is the rule.
        assert!(trace.contains("let gi = ddgi_irradiance_traced_plain(p, hit.n, -dir, limit);"));
        assert!(!trace.contains("let gi = ddgi_irradiance(p"));
        let wgsl = crate::ddgi_gpu::ddgi_shading_wgsl();
        assert!(wgsl.contains("if (c >= limit && inset >= 0.0) {"));
        assert!(wgsl.contains("return ddgi_irradiance_traced(p, n, v, 99u);"));
    }

    /// A move inside one cell changes the blend window but not what the probes trace: the
    /// probes still settle (and the update freezes) for a camera that only sways.
    #[test]
    fn a_sub_cell_move_keeps_the_probes_settled() {
        let mut state = DdgiState::new(DdgiSettings::default());
        for i in 0..6u32 {
            state.prepare(
                [0.3 + 0.2 * i as f32, 2.0, 0.4],
                &flat_light(),
                &[],
                &[],
                false,
                true,
            );
        }
        assert_eq!(
            state.settled_frames, 5,
            "inside one 2 m cell the volumes hold still"
        );
        state.prepare([2.3, 2.0, 0.4], &flat_light(), &[], &[], false, true);
        assert_eq!(state.settled_frames, 0, "a scroll is a new scene for the probes");
    }

    #[test]
    fn random_rotations_are_orthonormal() {
        for f in 0..32u32 {
            let r = random_rotation([hash01(f * 3), hash01(f * 3 + 1), hash01(f * 3 + 2)]);
            for (i, a) in r.iter().enumerate() {
                let row = Vec3::from_array(*a);
                assert!((row.length() - 1.0).abs() < 1e-4);
                for b in &r[i + 1..] {
                    assert!(row.dot(Vec3::from_array(*b)).abs() < 1e-4);
                }
            }
        }
    }

    #[test]
    fn data_layout_has_room_for_every_probe() {
        let s = DdgiSettings::default();
        let (irr, vis, hdr, packed, screen, end) = DdgiState::data_words(&s);
        assert_eq!(irr, s.probe_count() * 12);
        assert_eq!(vis - irr, s.probe_count() * 192);
        assert_eq!(hdr - vis, s.probe_count() * 256);
        assert_eq!(packed - hdr, s.probe_count() * 2);
        assert_eq!(screen - packed, s.probe_count() * 64);
        assert_eq!(end - screen, 1920 * 1080 * 8);
        // Fits one storage binding (128 MiB default limit).
        assert!(end * 4 < 128 << 20);
    }

    /// Audit sprint-audit-g-render4-platform (M): `clamped` must bound the probe buffers in
    /// bytes, not only the probe count, or `DdgiGpu::new` asks DX12 and non-NVIDIA Vulkan
    /// devices for a `create_buffer` past `max_buffer_size` / `max_storage_buffer_binding_size`
    /// and the first frame with such settings aborts.
    #[test]
    fn clamped_settings_keep_probe_buffers_within_one_storage_binding() {
        for settings in [
            DdgiSettings {
                dims: [1024, 1024, 1024],
                cascades: 4,
                ..DdgiSettings::default()
            },
            DdgiSettings {
                dims: [4096, 1024, 1024],
                cascades: 1,
                ..DdgiSettings::default()
            },
        ] {
            let c = settings.clamped();
            let (_, _, _, _, _, end) = DdgiState::data_words(&c);
            assert!(
                end * 4 <= u64::from(DdgiSettings::MAX_BINDING_BYTES),
                "ddgi data is {} bytes for {c:?}",
                end * 4
            );
            let rays = c.probe_count() * u64::from(RAYS_PER_PROBE) * 16;
            assert!(
                rays <= u64::from(DdgiSettings::MAX_BINDING_BYTES),
                "ddgi rays is {rays} bytes for {c:?}"
            );
        }
    }

    /// `clamped` bounded the probe buffers but not the brick pool, which `DdgiGpu` also creates
    /// and binds whole (`sdf_pool_values() * 4` bytes): a 1 GiB pool asked for a buffer past the
    /// storage-binding floor. The default and every tier keep their pool.
    #[test]
    fn clamped_settings_keep_the_sdf_pool_within_one_storage_binding() {
        let floor = u64::from(DdgiSettings::MAX_BINDING_BYTES);
        for bytes in [1u64 << 30, u64::MAX, floor + 4] {
            let c = DdgiSettings {
                sdf_pool_bytes: bytes,
                ..DdgiSettings::default()
            }
            .clamped();
            assert!(
                c.sdf_pool_bytes <= floor,
                "{bytes} clamps to {}",
                c.sdf_pool_bytes
            );
            assert!(c.sdf_pool_values() * 4 <= floor, "{bytes}");
            assert_eq!(
                c.sdf_pool_bytes, floor,
                "{bytes} keeps the largest pool that binds"
            );
        }
        let defaults = DdgiSettings::default();
        assert_eq!(defaults.clone().clamped().sdf_pool_bytes, defaults.sdf_pool_bytes);
        let at_floor = DdgiSettings {
            sdf_pool_bytes: floor,
            ..DdgiSettings::default()
        };
        assert_eq!(at_floor.clamped().sdf_pool_bytes, floor);
    }

    #[test]
    fn lookup_resolution_steps_and_sizes() {
        for (r, step) in [
            (GiLookupResolution::Full, 1),
            (GiLookupResolution::Half, 2),
            (GiLookupResolution::Quarter, 4),
        ] {
            assert_eq!(r.step(), step);
            assert_eq!(GiLookupResolution::from_step(step), Some(r));
        }
        assert_eq!(GiLookupResolution::from_step(3), None);
        assert_eq!(GiLookupResolution::default(), GiLookupResolution::Half);
        // Odd sizes round up, so the last texel row and column still cover the frame's edge.
        assert_eq!(GiLookupResolution::Full.lookup_size(1281, 721), (1281, 721));
        assert_eq!(GiLookupResolution::Half.lookup_size(1281, 721), (641, 361));
        assert_eq!(GiLookupResolution::Quarter.lookup_size(1281, 721), (321, 181));
        assert_eq!(GiLookupResolution::Half.lookup_size(2560, 1440), (1280, 720));
        assert_eq!(GiLookupResolution::Quarter.lookup_size(1920, 1080), (480, 270));
    }

    /// ADR-2605: a tier reserves its own screen lookup region. The probe buffer ends that much
    /// sooner, a frame whose half-resolution lookup fits keeps it, and a larger one takes a quarter
    /// lookup instead of none. The default region keeps its rule (an 8K frame: no lookup).
    #[test]
    fn a_smaller_lookup_region_shrinks_the_buffer_and_falls_back_to_quarter() {
        let high = DdgiSettings::default();
        let low = crate::DynamicGiTier::Low.settings();
        assert_eq!(high.screen_region(), (1920, 1080));
        assert_eq!(high.screen_bytes(), 1920 * 1080 * 32);
        assert_eq!(low.screen_region(), (960, 540));
        let (.., low_screen, low_end) = DdgiState::data_words(&low);
        assert_eq!((low_end - low_screen) * 4, low.screen_bytes());
        assert_eq!(low.screen_bytes(), 960 * 540 * 32);
        let (.., high_screen, high_end) = DdgiState::data_words(&high);
        assert_eq!((high_end - high_screen) * 4, high.screen_bytes());
        let mut state = DdgiState::new(low);
        // The Deck's 1280 × 800 and a 1080p frame keep their half-resolution lookup.
        state.set_screen(Some((1280, 800)), GiLookupResolution::Half);
        assert_eq!(state.uniform.screen[1..], [640, 400, 2]);
        state.set_screen(Some((1920, 1080)), GiLookupResolution::Half);
        assert_eq!(state.uniform.screen[1..], [960, 540, 2]);
        // 1440p does not fit at half: quarter, not off.
        state.set_screen(Some((2560, 1440)), GiLookupResolution::Half);
        assert_eq!(state.uniform.screen[1..], [640, 360, 4]);
        state.set_screen(Some((3840, 2160)), GiLookupResolution::Full);
        assert_eq!(state.uniform.screen[1..], [960, 540, 4]);
        // Past even quarter: off.
        state.set_screen(Some((7680, 4320)), GiLookupResolution::Half);
        assert!(!state.uniform_screen_on());
        // The default region: 8K stays off, as `a_lookup_the_region_refuses_falls_back_to_half` pins.
        let mut state = DdgiState::new(high);
        state.set_screen(Some((7680, 4320)), GiLookupResolution::Half);
        assert!(!state.uniform_screen_on());
        // An absurd region clamps to the default's.
        let huge = DdgiSettings {
            screen_max: [1 << 20, 0],
            ..DdgiSettings::default()
        };
        assert_eq!(huge.screen_region(), (1920, 1));
    }

    /// The default probes carry no seeding or visibility-blend bit, so frames that never name a
    /// tier, Desktop High's and Ultra's, shade as before (ADR-2605); Low and Medium set both.
    #[test]
    fn seeding_and_the_visibility_blend_are_off_by_default_and_on_for_every_tier() {
        let flags = |s: &DdgiSettings| {
            let mut state = DdgiState::new(s.clone());
            state.prepare([0.0; 3], &flat_light(), &[], &[], false, true);
            state.uniform.ctrl[3]
        };
        let default = flags(&DdgiSettings::default());
        assert_eq!(default & (CTRL_VISIBILITY_BLEND | (15 << CTRL_SEED_SHIFT)), 0);
        // ADR-5001: High and Ultra turn them on too.
        for tier in crate::DynamicGiTier::ALL {
            let bits = flags(&tier.settings());
            assert_ne!(bits & CTRL_VISIBILITY_BLEND, 0, "{tier:?}");
            assert_eq!(
                (bits >> CTRL_SEED_SHIFT) & 15,
                crate::dynamic_gi::LOW_SEED_UPDATES,
                "{tier:?}"
            );
        }
        // The weight clamps to the four bits the shaders read, and leaves the bits above alone.
        let heavy = DdgiSettings {
            seed_updates: 99,
            ..DdgiSettings::default()
        };
        let bits = flags(&heavy);
        assert_eq!((bits >> CTRL_SEED_SHIFT) & 15, MAX_SEED_UPDATES);
        assert_eq!(bits >> (CTRL_SEED_SHIFT + 4), 0);
    }

    /// ADR-5566, ADR-5716: the front weights' bit reaches the shaders on every tier and the edge
    /// visibility's on the Low and Medium tiers alone; a layout that sets neither keeps the lookup it had.
    #[test]
    fn the_front_weights_bit_is_set_on_every_tier_and_the_edge_bit_on_the_2_m_tiers() {
        let flags = |s: &DdgiSettings| {
            let mut state = DdgiState::new(s.clone());
            state.prepare([0.0; 3], &flat_light(), &[], &[], false, true);
            state.uniform.ctrl[3]
        };
        assert_eq!(
            flags(&DdgiSettings::default()) & (CTRL_FRONT_WEIGHTS | CTRL_EDGE_VISIBILITY),
            0
        );
        for tier in crate::DynamicGiTier::ALL {
            let bits = flags(&tier.settings());
            assert_ne!(bits & CTRL_FRONT_WEIGHTS, 0, "{tier:?}");
            assert_eq!(
                bits & CTRL_EDGE_VISIBILITY != 0,
                matches!(tier, crate::DynamicGiTier::Low | crate::DynamicGiTier::Medium),
                "{tier:?}"
            );
        }
        let host = DdgiSettings {
            front_weights: true,
            ..DdgiSettings::default()
        };
        let bits = flags(&host);
        assert_ne!(bits & CTRL_FRONT_WEIGHTS, 0, "a host layout may take the option");
        assert_eq!(bits & CTRL_EDGE_VISIBILITY, 0, "each option has its own bit");
        let host = DdgiSettings {
            edge_visibility: true,
            ..DdgiSettings::default()
        };
        let bits = flags(&host);
        assert_eq!(bits & CTRL_FRONT_WEIGHTS, 0, "each option has its own bit");
        assert_ne!(bits & CTRL_EDGE_VISIBILITY, 0);
        // The bits are their own: they overlap each other and no other field of `ctrl.w`.
        assert_eq!(CTRL_FRONT_WEIGHTS & CTRL_EDGE_VISIBILITY, 0);
        assert_eq!(
            (CTRL_FRONT_WEIGHTS | CTRL_EDGE_VISIBILITY)
                & (CTRL_VISIBILITY_BLEND
                    | CTRL_RAY_KEEPS_CASCADE
                    | (15 << CTRL_SEED_SHIFT)
                    | (3 << CTRL_RECENT_SHIFT)
                    | 0xffff),
            0
        );
    }

    #[test]
    fn a_lookup_the_region_refuses_falls_back_to_half() {
        let region = |w: u32, h: u32| w <= SCREEN_MAX.0 && h <= SCREEN_MAX.1;
        let full = GiLookupResolution::Full;
        // A full-resolution lookup fits the probe region up to 1920 × 1080, not at 1440p.
        assert_eq!(fit_lookup((1920, 1080), full, region), Some((1920, 1080, 1)));
        assert_eq!(fit_lookup((2560, 1440), full, region), Some((1280, 720, 2)));
        assert_eq!(
            fit_lookup((2560, 1440), GiLookupResolution::Quarter, region),
            Some((640, 360, 4))
        );
        // Larger than the region even at half resolution (an 8K frame), or an empty frame: off.
        assert_eq!(fit_lookup((7680, 4320), GiLookupResolution::Half, region), None);
        assert_eq!(fit_lookup((0, 720), GiLookupResolution::Half, region), None);

        // The uniform the forward and terrain passes read: size and step, zeros when off.
        let mut state = DdgiState::new(DdgiSettings::default());
        state.set_screen(Some((2560, 1440)), GiLookupResolution::Half);
        assert_eq!(state.uniform.screen[1..], [1280, 720, 2]);
        assert!(state.uniform_screen_on());
        state.set_screen(Some((2560, 1440)), GiLookupResolution::Full);
        assert_eq!(state.uniform.screen[1..], [1280, 720, 2]);
        state.set_screen(Some((1920, 1080)), GiLookupResolution::Full);
        assert_eq!(state.uniform.screen[1..], [1920, 1080, 1]);
        state.set_screen(Some((1920, 1080)), GiLookupResolution::Quarter);
        assert_eq!(state.uniform.screen[1..], [480, 270, 4]);
        assert_eq!(state.screen_half(), (480, 270));
        state.set_screen(None, GiLookupResolution::Half);
        assert_eq!(state.uniform.screen, [0; 4]);
        assert!(!state.uniform_screen_on());
        assert_eq!(state.screen_region(), [0; 3]);
    }

    #[test]
    fn a_light_jump_restarts_the_probes_and_a_moving_sun_does_not() {
        let light = |deg: f32, lux: f32| DdgiLight {
            to_sun: Some([deg.to_radians().cos(), deg.to_radians().sin(), 0.0]),
            sun: [lux; 3],
            sky_scale: 1.0,
            sky_lod: 0.0,
            flat_sky: [0.0; 3],
            fill: None,
            ground: None,
            extra: [None; crate::MAX_EXTRA_DIRECTIONAL],
        };
        // The clock moving the sun 0.25° a frame, or a cloud dimming it by a tenth: no cut.
        assert!(!light_cut(&light(40.0, 1000.0), &light(40.25, 1000.0)));
        assert!(!light_cut(&light(40.0, 1000.0), &light(40.0, 900.0)));
        // A time-of-day set: the sun jumps 30°, sets, or the level's light halves.
        assert!(light_cut(&light(40.0, 1000.0), &light(70.0, 1000.0)));
        assert!(light_cut(
            &light(40.0, 1000.0),
            &DdgiLight {
                to_sun: None,
                ..light(40.0, 0.0)
            }
        ));
        assert!(light_cut(&light(40.0, 1000.0), &light(40.0, 500.0)));
        // A planet-shine fill coming on (or its strength halving) re-converges them as well.
        let lit = DdgiLight {
            fill: Some([[0.0, 1.0, 0.0], [1600.0; 3]]),
            ..light(40.0, 1000.0)
        };
        assert!(light_cut(&light(40.0, 1000.0), &lit));
        assert!(light_cut(
            &lit,
            &DdgiLight {
                fill: Some([[0.0, 1.0, 0.0], [800.0; 3]]),
                ..lit
            }
        ));
        assert!(!light_cut(&lit, &lit));
        // The image light's ground plane set, taken away, moved or dimmed (ADR-5716): the lower half
        // of every probe's view changes at once.
        let grounded = DdgiLight {
            ground: Some(([0.0, 80.0, 0.0, 0.0], [120.0; 3])),
            ..light(40.0, 1000.0)
        };
        assert!(light_cut(&light(40.0, 1000.0), &grounded));
        assert!(light_cut(&grounded, &light(40.0, 1000.0)));
        assert!(!light_cut(&grounded, &grounded));
        assert!(light_cut(
            &grounded,
            &DdgiLight {
                ground: Some(([0.5, 80.0, 0.0, 0.0], [120.0; 3])),
                ..grounded
            }
        ));
        assert!(light_cut(
            &grounded,
            &DdgiLight {
                ground: Some(([0.0, 80.0, 0.0, 0.0], [60.0; 3])),
                ..grounded
            }
        ));
        assert!(!light_cut(
            &grounded,
            &DdgiLight {
                ground: Some(([0.0, 80.0, 0.0, 0.0], [110.0; 3])),
                ..grounded
            }
        ));
        // An extra directional light coming on, turning or halving re-converges them too (ADR-7254).
        let extra = |toward: [f32; 3], lux: f32| DdgiLight {
            extra: [Some([toward, [lux; 3]]), None, None],
            ..light(40.0, 1000.0)
        };
        assert!(light_cut(&light(40.0, 1000.0), &extra([0.0, 1.0, 0.0], 500.0)));
        assert!(light_cut(
            &extra([0.0, 1.0, 0.0], 500.0),
            &extra([0.0, 1.0, 0.0], 250.0)
        ));
        assert!(light_cut(
            &extra([0.0, 1.0, 0.0], 500.0),
            &extra([0.6, 0.8, 0.0], 500.0)
        ));
        assert!(!light_cut(
            &extra([0.0, 1.0, 0.0], 500.0),
            &extra([0.0, 1.0, 0.0], 480.0)
        ));
    }

    #[test]
    fn extra_directional_rows_become_probe_lights_toward_the_light() {
        let mut rows = [[0.0f32; 4]; 2 * crate::MAX_EXTRA_DIRECTIONAL];
        // Slot 0: travelling straight down, 300 lux of red. Slot 1: pointing up (below the
        // horizon), off. Slot 2: empty.
        rows[0] = [0.0, -2.0, 0.0, 1.0];
        rows[1] = [300.0, 0.0, 0.0, 0.0];
        rows[2] = [0.0, 1.0, 0.0, 1.0];
        rows[3] = [100.0; 4];
        let extra = DdgiLight::extra_from_rows(&rows);
        assert_eq!(extra[0], Some([[0.0, 1.0, 0.0], [300.0, 0.0, 0.0]]));
        assert_eq!(extra[1], None, "a light from below the horizon lights no probe");
        assert_eq!(extra[2], None);
        assert_eq!(
            DdgiLight::extra_from_rows(&[[0.0; 4]; 2 * crate::MAX_EXTRA_DIRECTIONAL]),
            [None; crate::MAX_EXTRA_DIRECTIONAL]
        );
    }

    #[test]
    fn the_probe_uniform_carries_the_extra_lights_and_they_move_the_scene_key() {
        let mut state = DdgiState::new(DdgiSettings::default());
        state.prepare([0.0, 2.0, 8.0], &flat_light(), &[], &[], false, true);
        assert_eq!(
            state.uniform.extra_dir,
            [[0.0; 4]; 2 * crate::MAX_EXTRA_DIRECTIONAL]
        );
        let key = state.scene_key;
        let lit = DdgiLight {
            extra: [None, Some([[0.0, 1.0, 0.0], [250.0; 3]]), None],
            ..flat_light()
        };
        state.prepare([0.0, 2.0, 8.0], &lit, &[], &[], false, true);
        assert_eq!(state.uniform.extra_dir[2], [0.0, 1.0, 0.0, 1.0]);
        assert_eq!(state.uniform.extra_dir[3], [250.0, 250.0, 250.0, 0.0]);
        assert_eq!(state.uniform.extra_dir[0], [0.0; 4], "slot 0 stays off");
        assert_ne!(state.scene_key, key, "the probes re-trace under the new light");
    }

    /// ADR-5716: the image light's ground plane reaches the trace shader in components of the uniform
    /// nothing else reads, so the buffer keeps its size (the frame-work ratchet counts its bytes), a
    /// frame without a plane writes zeros there, and a plane that changes wakes settled probes.
    #[test]
    fn the_ground_plane_rides_the_uniforms_spare_components() {
        // The plane adds no row: 17 rows plus only ADR-7254's six `extra_dir` rows (400 bytes before
        // that ADR; integ-post05b, ADR-7310).
        assert_eq!(std::mem::size_of::<DdgiUniformGpu>(), 4 * 32 + 16 * 23);
        let mut state = DdgiState::new(DdgiSettings::default());
        let plain = flat_light();
        state.prepare([0.0; 3], &plain, &[], &[], false, true);
        let u = state.uniform;
        assert_eq!(u.ctrl[3] & CTRL_GROUND_PLANE, 0);
        assert_eq!(
            [u.rot[0][3], u.rot[1][3], u.rot[2][3], u.sun[3], u.fill[3]],
            [0.0; 5]
        );
        assert_eq!([u.emitters[2], u.emitters[3]], [0, 0]);
        state.prepare([0.0; 3], &plain, &[], &[], false, true);
        let settled = state.settled_frames;
        assert!(settled > 0, "an unchanged scene settles");
        let grounded = DdgiLight {
            ground: Some(([0.25, 80.0, 1.0, -2.0], [30.0, 20.0, 10.0])),
            ..plain
        };
        state.prepare([0.0; 3], &grounded, &[], &[], false, true);
        let u = state.uniform;
        assert_ne!(u.ctrl[3] & CTRL_GROUND_PLANE, 0);
        assert_eq!(u.ctrl[3] & (1 << 12), 1 << 12, "a plane set is a light cut");
        assert_eq!(
            [u.rot[0][3], u.rot[1][3], u.rot[2][3], u.sun[3]],
            [0.25, 80.0, 1.0, -2.0]
        );
        assert_eq!(
            [
                u.fill[3],
                f32::from_bits(u.emitters[2]),
                f32::from_bits(u.emitters[3])
            ],
            [30.0, 20.0, 10.0]
        );
        assert_eq!(state.settled_frames, 0, "a new plane wakes the probes");
        // The bit is its own.
        assert_eq!(
            CTRL_GROUND_PLANE
                & (CTRL_FRONT_WEIGHTS
                    | CTRL_EDGE_VISIBILITY
                    | CTRL_VISIBILITY_BLEND
                    | CTRL_RAY_KEEPS_CASCADE
                    | (15 << CTRL_SEED_SHIFT)
                    | (3 << CTRL_RECENT_SHIFT)
                    | 0xffff),
            0
        );
    }

    #[test]
    fn screen_texel_rebuilds_the_ambient_cube() {
        // Axis normals read their own slot; others blend by the squared components.
        let texel = DdgiScreenTexel {
            e: [[1.0; 3], [2.0; 3], [3.0; 3], [4.0; 3], [5.0; 3], [6.0; 3]],
            coverage: 1.0,
        };
        assert_eq!(texel.at([0.0, 1.0, 0.0])[0], 3.0);
        assert_eq!(texel.at([0.0, -1.0, 0.0])[0], 4.0);
        assert_eq!(texel.at([0.0, 0.0, -1.0])[0], 6.0);
        let tilted = texel.at([0.6, 0.0, 0.8])[0];
        assert!((tilted - (0.36 * 1.0 + 0.64 * 5.0)).abs() < 1e-5, "{tilted}");
        assert_eq!(unpack_rgb9e5(0), [0.0; 3]);
    }

    fn flat_light() -> DdgiLight {
        DdgiLight {
            to_sun: Some([0.0, 1.0, 0.0]),
            sun: [1.0; 3],
            sky_scale: 0.0,
            sky_lod: 0.0,
            flat_sky: [0.2; 3],
            fill: None,
            ground: None,
            extra: [None; crate::MAX_EXTRA_DIRECTIONAL],
        }
    }

    fn step(state: &mut DdgiState, meshes: &[FrameMesh<'_>]) {
        state.prepare([0.0, 2.0, 8.0], &flat_light(), meshes, &[], false, true);
    }

    fn proxy<'a>(
        key: u64,
        vertices: &'a [crate::LabVertex],
        indices: &'a [u32],
        material: &'a crate::LabMaterial,
    ) -> FrameMesh<'a> {
        FrameMesh {
            is_static: false,
            mesh_key: key,
            material_key: 7,
            object_key: key,
            vertices,
            indices,
            material,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        }
    }

    /// 4 m × 0.2 m × 0.2 m bar: 43 × 7 × 7 distance values (ADR-5338), one brick of the pool tests.
    fn thin_bar() -> (Vec<crate::LabVertex>, Vec<u32>, u64) {
        let (vertices, indices) = crate::box_mesh([2.0, 0.1, 0.1]);
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let count = brick_value_count(&positions, &indices).expect("thin bar bakes");
        assert_eq!(count, 43 * 7 * 7, "pool tests assume this brick size");
        (vertices, indices, count)
    }

    fn pool_of(bricks: u64, frames: u32) -> DdgiSettings {
        let (_, _, count) = thin_bar();
        DdgiSettings {
            sdf_pool_bytes: count * 4 * bricks,
            sdf_evict_frames: frames,
            bake_on_caller: true,
            ..DdgiSettings::default()
        }
    }

    #[test]
    fn sdf_pool_defaults_match_the_old_32_mb_budget() {
        let settings = DdgiSettings::default();
        assert_eq!(SDF_POOL_VALUES, 8 << 20);
        assert_eq!(SDF_POOL_BYTES, SDF_POOL_VALUES * 4);
        assert_eq!(SDF_POOL_BYTES, 32 << 20);
        assert_eq!(settings.sdf_pool_bytes, SDF_POOL_BYTES);
        assert_eq!(settings.sdf_pool_values(), SDF_POOL_VALUES);
        assert_eq!(settings.sdf_evict_frames, SDF_EVICT_FRAMES);
        assert_eq!(SDF_EVICT_FRAMES, 300);
        // Below 1 acts as 1: a drawn mesh is never dropped on the frame it is drawn.
        let state = DdgiState::new(DdgiSettings {
            sdf_evict_frames: 0,
            ..DdgiSettings::default()
        });
        assert_eq!(state.evict_after(), 1);
    }

    #[test]
    fn a_full_pool_evicts_the_least_recent_mesh_and_traces_the_new_one() {
        let (vertices, indices, count) = thin_bar();
        let material = crate::LabMaterial::default();
        let mut state = DdgiState::new(pool_of(1, 2));
        let a = proxy(1, &vertices, &indices, &material);
        let b = proxy(2, &vertices, &indices, &material);
        // Frame 1–2: A is drawn and keeps offset 0.
        step(&mut state, &[a]);
        assert!(state.brick_resident(1) && state.brick_traced(1));
        assert_eq!(state.brick_offset(1), Some(0));
        step(&mut state, &[a]);
        assert_eq!(state.brick_offset(1), Some(0));
        // One frame of only B: A is one frame inside the window, and B does not fit.
        step(&mut state, &[b]);
        assert!(state.brick_resident(1), "age 1 is inside a window of 2");
        assert!(!state.brick_resident(2));
        assert!(!state.brick_traced(2));
        // The next frame A is old enough. Its span returns to the bump, and B is traced there.
        // B was not recorded as a permanent failure: it bakes as soon as the span is free.
        step(&mut state, &[b]);
        assert!(!state.brick_resident(1));
        assert!(state.brick_resident(2) && state.brick_traced(2));
        assert_eq!(state.brick_offset(2), Some(0));

        // Two-brick pool, window 3. A leaves on frame 2; on frame 5 its age is 3, so C
        // takes A's hole and B's offset stays.
        let mut state = DdgiState::new(pool_of(2, 3));
        let c = proxy(3, &vertices, &indices, &material);
        step(&mut state, &[a]);
        step(&mut state, &[a, b]);
        assert_eq!(state.brick_offset(1), Some(0));
        assert_eq!(state.brick_offset(2), Some(count as u32));
        step(&mut state, &[b]);
        step(&mut state, &[b]);
        assert!(state.brick_resident(1), "age 2 is inside a window of 3");
        step(&mut state, &[b, c]);
        assert!(!state.brick_resident(1), "A is the least recently drawn");
        assert_eq!(state.brick_offset(2), Some(count as u32));
        assert!(state.brick_traced(2));
        assert_eq!(state.brick_offset(3), Some(0));
        assert!(state.brick_traced(3));
    }

    /// ADR-5716: a foliage material that adopted a sway keeps its maps in its graph's slots, and the
    /// probes read them there: a plant placed in the frame's mesh list reflects what the same plant
    /// in an instance set does (the set draws under the plain twin, whose maps are back in the
    /// standard slots). Until then the placed plant reflected its base colour factor alone.
    #[test]
    fn an_adopted_foliage_material_gives_the_probes_its_maps_means() {
        let texels = |rgba: [u8; 4]| crate::LabTexture {
            width: 4,
            height: 4,
            rgba: rgba.repeat(16),
        };
        let plain = crate::LabMaterial {
            base_color: [1.0, 0.5, 1.0, 1.0],
            albedo: Some(texels([128, 64, 32, 255])),
            emissive: [1.0, 1.0, 1.0],
            emissive_intensity: 40.0,
            emissive_map: Some(texels([255, 128, 0, 255])),
            ..crate::LabMaterial::default()
        };
        let adopted = crate::FoliageSway::default().adopt(&plain).expect("adopt");
        assert!(
            adopted.albedo.is_none() && adopted.emissive_map.is_none(),
            "the maps live in the graph's slots"
        );
        let twin = crate::FoliageSway::plain(&adopted).expect("the plain twin");
        let mut state = DdgiState::new(DdgiSettings::default());
        let albedo = state.mean_albedo_of(&plain, 7);
        let emissive = state.mean_emissive_of(&plain, 7);
        assert!(
            albedo[0] < 0.3 && albedo[1] < 0.05 && albedo[2] < 0.02,
            "the map darkens the base colour: {albedo:?}"
        );
        assert!(emissive[0] > 39.0 && emissive[2] < 1e-3, "{emissive:?}");
        // Keyed (cached per key) and unkeyed (read each time).
        for key in [8, 0] {
            assert_eq!(state.mean_albedo_of(&adopted, key), albedo, "key {key}");
            assert_eq!(state.mean_emissive_of(&adopted, key), emissive, "key {key}");
        }
        assert_eq!(state.mean_albedo_of(&twin, 9), albedo);
        assert_eq!(state.mean_emissive_of(&twin, 9), emissive);
        // Another graph's slots are its own: only the adopted standard surface is read.
        let mut other = adopted.clone();
        let mut graph = (**other.surface_graph.as_ref().expect("graph")).clone();
        graph.wgsl = "fn material_surface() {}".into();
        other.surface_graph = Some(std::sync::Arc::new(graph));
        assert_eq!(state.mean_albedo_of(&other, 10), [0.95, 0.5, 0.95]);
    }

    fn set_of<'a>(
        key: u64,
        positions: &'a [[f32; 3]],
        normals: &'a [[f32; 3]],
        indices: &'a [u32],
        material: &'a crate::LabMaterial,
        models: &'a [[[f32; 4]; 4]],
    ) -> ProbeSet<'a> {
        ProbeSet {
            set_key: key,
            generation: 1,
            mesh_key: key,
            material_key: 7,
            material,
            positions,
            normals,
            indices,
            models,
            shadow: ShadowCast::All,
            shadows: &[],
            scale_range: (1.0, 1.0),
        }
    }

    /// ADR-5457: a persistent instance set enters the probes' scene as the same mesh placed in the
    /// frame's mesh list does: the same brick, the same instance record, the same cascade lists.
    /// Before, the probes took the frame's mesh list alone, and a mesh drawn as a set neither
    /// occluded nor bounced light for them (`visbuffer_voxel_far`'s placed canopy against its
    /// instance set: 96.6 % of the frame equal).
    #[test]
    fn an_instance_set_enters_the_probe_scene_like_a_placed_mesh() {
        let (vertices, indices) = crate::box_mesh([1.5, 1.0, 0.5]);
        let (ground_vertices, ground_indices) = crate::box_mesh([20.0, 0.05, 20.0]);
        let material = crate::LabMaterial {
            base_color: [0.3, 0.6, 0.2, 1.0],
            ..crate::LabMaterial::default()
        };
        let floor = crate::LabMaterial::default();
        let model = (Mat4::from_translation(Vec3::new(3.0, 1.0, -2.0)) * Mat4::from_rotation_y(0.4))
            .to_cols_array_2d();
        let settings = DdgiSettings {
            bake_on_caller: true,
            ..DdgiSettings::default()
        };
        let ground = proxy(1, &ground_vertices, &ground_indices, &floor);
        let mut placed_mesh = proxy(9, &vertices, &indices, &material);
        placed_mesh.model = model;
        let mut placed = DdgiState::new(settings.clone());
        step(&mut placed, &[ground, placed_mesh]);
        assert!(placed.brick_traced(9));

        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let models = [model];
        let set = set_of(9, &positions, &normals, &indices, &material, &models);
        let mut as_set = DdgiState::new(settings.clone());
        as_set.prepare_with_sets(
            [0.0, 2.0, 8.0],
            &flat_light(),
            &ProbeGeometry {
                meshes: &[ground],
                sets: &[set],
            },
            &[],
            false,
            true,
        );
        assert!(as_set.brick_resident(9) && as_set.brick_traced(9));
        assert_eq!(as_set.instances.len(), 2);
        assert_eq!(
            bytemuck::cast_slice::<SdfInstanceGpu, u8>(&as_set.instances),
            bytemuck::cast_slice::<SdfInstanceGpu, u8>(&placed.instances),
            "the set's instance is the placed mesh's record"
        );
        assert_eq!(as_set.lists, placed.lists);
        assert_eq!(as_set.uniform.inst_count, placed.uniform.inst_count);
        assert_eq!(as_set.uniform.inst_offset, placed.uniform.inst_offset);
        assert_eq!(
            as_set.scene_key, placed.scene_key,
            "the probes settle on the same scene"
        );

        // The frame's mesh list alone: what the probes traced for a set before.
        let mut without = DdgiState::new(settings);
        step(&mut without, &[ground]);
        assert_eq!(without.instances.len(), 1);
        assert!(!without.brick_traced(9));
    }

    /// A set's instances follow the placed mesh's rules one by one (ADR-5457): one that casts no
    /// shadow is not traced, one beyond every cascade's reach is not listed, a material the probes
    /// skip keeps the whole set out, and past the instance buffer's room the largest at the eye
    /// stay.
    #[test]
    fn set_instances_follow_the_shadow_policy_the_reach_and_the_room() {
        let (vertices, indices) = crate::box_mesh([1.5, 1.0, 0.5]);
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let material = crate::LabMaterial::default();
        let at = |x: f32| Mat4::from_translation(Vec3::new(x, 1.0, 0.0)).to_cols_array_2d();
        let settings = DdgiSettings {
            bake_on_caller: true,
            ..DdgiSettings::default()
        };
        let prepare = |state: &mut DdgiState, set: ProbeSet<'_>| {
            state.prepare_with_sets(
                [0.0, 2.0, 0.0],
                &flat_light(),
                &ProbeGeometry {
                    meshes: &[],
                    sets: &[set],
                },
                &[],
                false,
                true,
            );
        };

        // Shadow policy and reach.
        let models = [at(4.0), at(1.0e6), at(-4.0)];
        let shadows = [ShadowCast::All, ShadowCast::All, ShadowCast::Off];
        let mut set = set_of(9, &positions, &normals, &indices, &material, &models);
        set.shadows = &shadows;
        let mut state = DdgiState::new(settings.clone());
        prepare(&mut state, set);
        assert_eq!(state.instances.len(), 1, "the caster within reach alone");
        assert!((state.instances[0].aabb_min[0] - 2.5).abs() < 1.0);

        // A set that casts no shadow at all, and an unlit one, get no brick.
        let mut off = set_of(9, &positions, &normals, &indices, &material, &models);
        off.shadow = ShadowCast::Off;
        let mut state = DdgiState::new(settings.clone());
        prepare(&mut state, off);
        assert!(state.instances.is_empty() && !state.brick_resident(9));
        let unlit = crate::LabMaterial {
            unlit: true,
            ..crate::LabMaterial::default()
        };
        let mut state = DdgiState::new(settings.clone());
        prepare(
            &mut state,
            set_of(9, &positions, &normals, &indices, &unlit, &models),
        );
        assert!(state.instances.is_empty() && !state.brick_resident(9));
        // The host's opt-out covers a set's mesh too.
        let mut state = DdgiState::new(settings.clone());
        state.set_gi_proxy_off([9]);
        prepare(
            &mut state,
            set_of(9, &positions, &normals, &indices, &material, &models),
        );
        assert!(state.instances.is_empty() && !state.brick_resident(9));

        // The pick is kept from frame to frame, and made again when the sets change.
        let two = [at(4.0), at(8.0)];
        let mut set = set_of(9, &positions, &normals, &indices, &material, &two);
        let mut state = DdgiState::new(settings.clone());
        prepare(&mut state, set);
        let first = state.instances.clone();
        prepare(&mut state, set);
        assert_eq!(
            bytemuck::cast_slice::<SdfInstanceGpu, u8>(&state.instances),
            bytemuck::cast_slice::<SdfInstanceGpu, u8>(&first)
        );
        let moved = [at(4.0), at(1.0e6), at(12.0)];
        set.models = &moved;
        set.generation = 2;
        prepare(&mut state, set);
        assert_eq!(state.instances.len(), 2, "the new set's instances in reach");
        let last = state.instances[1];
        assert!((0.5 * (last.aabb_min[0] + last.aabb_max[0]) - 12.0).abs() < 0.01);

        // Room: ten instances more than the buffer holds, a metre apart along x from the eye.
        let many: Vec<[[f32; 4]; 4]> = (0..MAX_INSTANCES + 10).map(|i| at(i as f32)).collect();
        let mut state = DdgiState::new(settings);
        prepare(
            &mut state,
            set_of(9, &positions, &normals, &indices, &material, &many),
        );
        assert_eq!(state.instances.len(), MAX_INSTANCES);
        let farthest = state
            .instances
            .iter()
            .map(|i| 0.5 * (i.aabb_min[0] + i.aabb_max[0]))
            .fold(f32::MIN, f32::max);
        assert!(
            (farthest - (MAX_INSTANCES - 1) as f32).abs() < 0.5,
            "the ten farthest are the ones dropped: farthest kept centre {farthest}"
        );
        // Kept in the set's own order, so the list does not reshuffle as the eye moves.
        assert!(
            state
                .instances
                .windows(2)
                .all(|w| w[0].aabb_min[0] < w[1].aabb_min[0])
        );
    }

    /// Audit fix-render4 finding 2 (medium): emitter clusters come out in cell-coordinate
    /// order, not `HashMap` order, so equal-area cells (a regular grid) keep a deterministic
    /// order in the per-mesh cluster cache.
    #[test]
    fn emitter_clusters_come_out_in_cell_order() {
        // One equal-area triangle in each of four cells of a 4 m extent, inserted scrambled.
        let v = |p: [f32; 3]| crate::LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0; 2]);
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        for (i, cx) in [3.0f32, 1.0, 0.0, 2.0].iter().enumerate() {
            let base = i as u32 * 3;
            // The cell-0 triangle touches x = 0 and the cell-3 one touches x = 4, so the
            // extent is exactly 4 m and the grid cell is exactly 1 m.
            let (x0, x1) = if i == 2 {
                (*cx, cx + 0.2)
            } else if i == 0 {
                (cx - 0.2, *cx)
            } else {
                (cx - 0.1, cx + 0.1)
            };
            vertices.extend([
                v([x0, 0.0, 0.0]),
                v([x1, 0.0, 0.0]),
                v([cx - 0.1 + 0.1, 0.2, 0.0]),
            ]);
            indices.extend([base, base + 1, base + 2]);
        }
        let material = crate::LabMaterial::default();
        let mesh = proxy(1, &vertices, &indices, &material);
        let cells: Vec<[u32; 3]> = emitter_clusters(&mesh).iter().map(|c| c.cell).collect();
        assert_eq!(cells, vec![[0, 0, 0], [1, 0, 0], [2, 0, 0], [3, 0, 0]]);
    }

    /// Audit fix-render4 finding 2 (medium): with the emitter list one slot short, a bitwise
    /// strength tie at the cap keeps the lower cell coordinate, whatever order the cluster
    /// `HashMap` yielded them in.
    #[test]
    fn emitter_list_cap_keeps_the_lower_cell_of_bitwise_ties() {
        let eye = [2.0f32, 2.0, 8.0];
        // `n - 1` strictly stronger single-cluster meshes fill the list.
        let strong_tri: Vec<crate::LabVertex> = [[1.4, 0.0, 0.0], [1.6, 0.0, 0.0], [1.5, 0.2, 0.0]]
            .into_iter()
            .map(|p| crate::LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0; 2]))
            .collect();
        let strong_idx: Vec<u32> = vec![0, 1, 2];
        let strong_material = crate::LabMaterial {
            emissive: [1.0; 3],
            emissive_intensity: 2.0,
            ..crate::LabMaterial::default()
        };
        let twin_tri: Vec<crate::LabVertex> = [
            [1.4, 0.0, 0.0],
            [1.6, 0.0, 0.0],
            [1.5, 0.2, 0.0],
            [2.4, 0.0, 0.0],
            [2.6, 0.0, 0.0],
            [2.5, 0.2, 0.0],
        ]
        .into_iter()
        .map(|p| crate::LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0; 2]))
        .collect();
        let twin_idx: Vec<u32> = vec![0, 1, 2, 3, 4, 5];
        let twin_material = crate::LabMaterial {
            emissive: [1.0; 3],
            ..crate::LabMaterial::default()
        };
        // The two triangles mirror each other about the eye's x, so their strengths are
        // bitwise equal; only the cell coordinate differs (0 vs 3 inside this mesh's grid).
        let twin = proxy(999, &twin_tri, &twin_idx, &twin_material);
        let mut meshes: Vec<FrameMesh<'_>> = (0..MAX_EMITTERS - 1)
            .map(|i| proxy(i as u64 + 1, &strong_tri, &strong_idx, &strong_material))
            .collect();
        meshes.push(twin);
        let mut state = DdgiState::new(DdgiSettings::default());
        let list = state.emitter_list(eye.into(), &meshes, &[]);
        assert_eq!(list.len(), MAX_EMITTERS);
        let xs: Vec<f32> = list.iter().map(|e| e.pos_radius[0]).collect();
        assert!(
            xs.iter().any(|x| (x - 1.5).abs() < 1e-3),
            "the lower-cell twin survives: {xs:?}"
        );
        assert!(
            !xs.iter().any(|x| (x - 2.5).abs() < 1e-3),
            "the higher-cell twin is dropped: {xs:?}"
        );
    }

    /// Audit fix-render4 finding 2 (medium): a strength tie between two meshes keeps the
    /// earlier mesh in the frame, before the cell coordinate is compared.
    #[test]
    fn emitter_list_cap_keeps_the_earlier_mesh_of_bitwise_ties() {
        let eye = [2.0f32, 2.0, 8.0];
        let tri: Vec<crate::LabVertex> = [[1.4, 0.0, 0.0], [1.6, 0.0, 0.0], [1.5, 0.2, 0.0]]
            .into_iter()
            .map(|p| crate::LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0; 2]))
            .collect();
        // The mirror twin: bitwise-equal strength, distinguishable position.
        let mirror: Vec<crate::LabVertex> = [[2.4, 0.0, 0.0], [2.6, 0.0, 0.0], [2.5, 0.2, 0.0]]
            .into_iter()
            .map(|p| crate::LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0; 2]))
            .collect();
        let idx: Vec<u32> = vec![0, 1, 2];
        let material = crate::LabMaterial {
            emissive: [1.0; 3],
            ..crate::LabMaterial::default()
        };
        let mut meshes: Vec<FrameMesh<'_>> = (0..MAX_EMITTERS)
            .map(|i| proxy(i as u64 + 1, &tri, &idx, &material))
            .collect();
        meshes.push(proxy(998, &mirror, &idx, &material));
        let mut state = DdgiState::new(DdgiSettings::default());
        let list = state.emitter_list(eye.into(), &meshes, &[]);
        assert_eq!(list.len(), MAX_EMITTERS);
        let xs: Vec<f32> = list.iter().map(|e| e.pos_radius[0]).collect();
        assert!(
            xs.iter().any(|x| (x - 1.5).abs() < 1e-3),
            "the earlier mesh survives: {xs:?}"
        );
        assert!(
            !xs.iter().any(|x| (x - 2.5).abs() < 1e-3),
            "the later mesh is dropped: {xs:?}"
        );
    }

    /// Audit fix-render4 finding 1 (high): a sync bake stores every pending brick before the
    /// trace, in ascending mesh-key order, so the pool's first-fit offsets do not depend on
    /// the task pool's finish order or on `HashMap` iteration order. Two runs that finish
    /// the same bricks in either order must land the same resident set at the same offsets.
    #[test]
    fn sync_bake_stores_pending_bricks_in_key_order() {
        let brick = |len: usize| crate::ddgi_bake::SdfBrick {
            origin: [0.0; 3],
            voxel: [1.0; 3],
            dims: [1, 1, len as u32],
            values: vec![0.0; len],
        };
        let mut state = DdgiState::new(DdgiSettings {
            sdf_pool_bytes: 4 * 1024,
            bake_on_caller: false,
            ..DdgiSettings::default()
        });
        // A fragmented pool: one 45-value span, nothing bumped yet.
        state.free_spans = vec![FreeSpan { offset: 0, len: 45 }];
        state.pool_end = 45;
        // Five finished bakes the task pool finished in an unknown order. The sizes are
        // chosen so any store order other than ascending key parks at least one brick at a
        // different offset (or on the bump instead of the span).
        for (key, len) in [(1u64, 10usize), (2, 20), (3, 30), (4, 40), (5, 50)] {
            state
                .pending
                .insert(key, regia_core::tasks::spawn(async move { Some(brick(len)) }));
        }
        state.bake(&[], &[], &[], &[], true);
        let offsets: Vec<(u64, u32)> = (1..=5u64)
            .map(|k| (k, state.brick_offset(k).expect("resident")))
            .collect();
        assert_eq!(
            offsets,
            vec![(1, 0), (2, 10), (3, 45), (4, 75), (5, 115)],
            "ascending-key first-fit, independent of task-pool finish order"
        );
        assert!(state.pending.is_empty());
        // Uploads go out in the same normalized order.
        let upload_offsets: Vec<u64> = state.uploads.iter().map(|(at, _)| *at).collect();
        assert_eq!(upload_offsets, vec![0, 10, 45, 75, 115]);
    }

    /// Audit fix-render4 finding 1 (high), second half: with sync bakes every candidate mesh
    /// the frame draws is resident and traced on that same frame — the frame never traces a
    /// brick set the task pool's timing chose.
    #[test]
    fn sync_prepare_traces_every_candidate_on_its_frame() {
        let (vertices, indices, _) = thin_bar();
        let material = crate::LabMaterial::default();
        let mut state = DdgiState::new(pool_of(4, 10));
        let meshes: Vec<FrameMesh<'_>> = (1..=3u64)
            .map(|k| proxy(k, &vertices, &indices, &material))
            .collect();
        step(&mut state, &meshes);
        for k in 1..=3u64 {
            assert!(
                state.brick_resident(k),
                "mesh {k} is resident on the frame it is asked"
            );
            assert!(
                state.brick_traced(k),
                "mesh {k} is traced on the frame it is asked"
            );
        }
        assert_eq!(state.pending_bakes(), 0, "a sync frame waits for every bake");
    }

    #[test]
    fn a_released_mesh_frees_its_brick_at_once() {
        // One-brick pool, a long window: without the release B would wait 300 frames.
        let (vertices, indices, _) = thin_bar();
        let material = crate::LabMaterial::default();
        let mut state = DdgiState::new(pool_of(1, 300));
        let a = proxy(1, &vertices, &indices, &material);
        let b = proxy(2, &vertices, &indices, &material);
        step(&mut state, &[a]);
        assert!(state.brick_resident(1));
        state.failed.insert(1);
        state.clusters.insert(1, Vec::new());
        state.release_meshes(&[1]);
        assert!(!state.brick_resident(1));
        assert!(!state.failed.contains(&1) && !state.clusters.contains_key(&1));
        step(&mut state, &[b]);
        assert!(state.brick_resident(2) && state.brick_traced(2));
        assert_eq!(state.brick_offset(2), Some(0));
        // Keys it never saw are ignored.
        state.release_meshes(&[7, 8]);
        assert!(state.brick_resident(2));
    }

    #[test]
    fn an_opted_out_mesh_allocates_no_brick_and_drops_one_it_had() {
        let (vertices, indices, _) = thin_bar();
        let material = crate::LabMaterial::default();
        let mut state = DdgiState::new(pool_of(4, 300));
        let a = proxy(1, &vertices, &indices, &material);
        assert!(proxy_candidate(&a, &state.settings));
        step(&mut state, &[a]);
        assert!(state.brick_resident(1));
        state.set_gi_proxy_off([1]);
        step(&mut state, &[a]);
        assert!(!state.brick_resident(1), "opt-out frees the brick the same frame");
        assert!(!state.brick_traced(1));
        let b = proxy(2, &vertices, &indices, &material);
        assert_eq!(b.shadow, ShadowCast::All);
        state.set_gi_proxy_off([2]);
        step(&mut state, &[b]);
        assert!(!state.brick_resident(2));
        assert!(!state.brick_traced(2));
        let mut sky = proxy(3, &vertices, &indices, &material);
        sky.shadow = ShadowCast::Off;
        assert!(
            !proxy_candidate(&sky, &state.settings),
            "a sky dome stays out of the pool"
        );
    }

    /// Review w11 #2: a sorted hair card the host left `ALPHA_OPAQUE` is blended, so it is not a
    /// GI proxy (as an `ALPHA_BLEND` material is not); hashed hair still is.
    #[test]
    fn sorted_hair_left_opaque_is_not_a_proxy() {
        let (vertices, indices, _) = thin_bar();
        let mut material = crate::LabMaterial::default();
        let settings = pool_of(4, 300);
        material.hair = crate::HairFactors {
            alpha: crate::HairAlpha::Sorted,
            ..crate::HairFactors::strands()
        };
        assert_eq!(material.alpha_mode, crate::ALPHA_OPAQUE);
        assert!(!proxy_candidate(
            &proxy(1, &vertices, &indices, &material),
            &settings
        ));
        material.hair.alpha = crate::HairAlpha::Hashed;
        assert!(proxy_candidate(
            &proxy(1, &vertices, &indices, &material),
            &settings
        ));
    }

    #[test]
    fn a_mesh_drawn_every_frame_keeps_its_offset() {
        let (vertices, indices, _) = thin_bar();
        let material = crate::LabMaterial::default();
        let mut state = DdgiState::new(pool_of(1, 2));
        let a = proxy(1, &vertices, &indices, &material);
        for _ in 0..400 {
            step(&mut state, &[a]);
        }
        assert_eq!(state.brick_offset(1), Some(0));
        assert!(state.brick_traced(1));
    }

    /// A second prepare of the same meshes reuses the instance, list and emitter scratch,
    /// transforms each emissive cluster once, and does not build the parked-mesh set while
    /// nothing is parked.
    #[test]
    fn a_static_scene_reuses_instance_and_emitter_scratch() {
        let (vertices, indices, _) = thin_bar();
        let material = crate::LabMaterial {
            emissive: [4.0; 3],
            ..crate::LabMaterial::default()
        };
        let mut state = DdgiState::new(pool_of(2, 30));
        let mesh = proxy(1, &vertices, &indices, &material);
        let eye = [0.0, 2.0, 8.0];
        state.prepare(eye, &flat_light(), &[mesh], &[], false, true);
        let caps = (
            state.instances.capacity(),
            state.lists.capacity(),
            state.emitters.capacity(),
        );
        let growths = state.scratch_growths;
        let transforms = state.emitter_transforms;
        assert!(
            caps.0 > 0 && caps.1 > 0 && caps.2 > 0 && transforms > 0,
            "scratch filled: caps {caps:?}, transforms {transforms}"
        );
        assert!(!state.instances.is_empty() && !state.emitters.is_empty());
        assert_eq!(state.settled_frames, 0);
        state.prepare(eye, &flat_light(), &[mesh], &[], false, true);
        assert!(
            state.instances.capacity() == caps.0
                && state.lists.capacity() == caps.1
                && state.emitters.capacity() == caps.2
                && state.scratch_growths == growths
                && state.emitter_transforms == transforms
                && state.parked_set_builds == 0
                && state.settled_frames >= 1,
            "caps {caps:?} -> ({}, {}, {}), growths {growths} -> {}, transforms {transforms} -> {}, parked {}, settled {}",
            state.instances.capacity(),
            state.lists.capacity(),
            state.emitters.capacity(),
            state.scratch_growths,
            state.emitter_transforms,
            state.parked_set_builds,
            state.settled_frames,
        );
    }

    #[test]
    fn a_gap_shorter_than_the_window_keeps_the_brick() {
        let (vertices, indices, _) = thin_bar();
        let material = crate::LabMaterial::default();
        let mut state = DdgiState::new(pool_of(1, SDF_EVICT_FRAMES));
        let a = proxy(1, &vertices, &indices, &material);
        step(&mut state, &[a]);
        assert_eq!(state.brick_offset(1), Some(0));
        for _ in 0..(SDF_EVICT_FRAMES - 1) {
            step(&mut state, &[]);
        }
        assert_eq!(
            state.brick_offset(1),
            Some(0),
            "299 undrawn updates stay inside 300"
        );
        step(&mut state, &[]);
        assert!(!state.brick_resident(1), "the 300th undrawn update frees it");
    }

    #[test]
    fn a_camera_cut_keeps_a_brick_drawn_on_the_previous_update() {
        // A cut resets the probe clock. The age has to move with it: a raw subtract against
        // the new clock would evict every brick on the cut.
        let (vertices, indices, _) = thin_bar();
        let material = crate::LabMaterial::default();
        let mut state = DdgiState::new(pool_of(1, 2));
        let a = proxy(1, &vertices, &indices, &material);
        let b = proxy(2, &vertices, &indices, &material);
        step(&mut state, &[a]);
        step(&mut state, &[a]);
        state.restart();
        step(&mut state, &[b]);
        assert!(
            state.brick_resident(1),
            "one undrawn update after the cut stays inside the window"
        );
        assert!(!state.brick_resident(2), "B does not fit beside A");
        step(&mut state, &[b]);
        assert!(!state.brick_resident(1));
        assert!(state.brick_resident(2) && state.brick_traced(2));
        assert_eq!(state.brick_offset(2), Some(0));
    }

    #[test]
    fn a_full_pool_does_not_remeasure_a_mesh_that_does_not_fit_until_a_brick_is_freed() {
        // The refusal is cached at the pool generation. Later frames must not walk the
        // mesh. A free moves that generation; the cached size is reused and the waiting
        // mesh is traced. A budget change drops this state entirely (`DdgiGpu::matches`).
        let (vertices, indices, _) = thin_bar();
        let material = crate::LabMaterial::default();
        let mut state = DdgiState::new(pool_of(1, 300));
        let a = proxy(1, &vertices, &indices, &material);
        let b = proxy(2, &vertices, &indices, &material);
        let c = proxy(3, &vertices, &indices, &material);
        step(&mut state, &[a]);
        step(&mut state, &[a, b, c]);
        let walked = state.fit_walks();
        assert_eq!(walked, 3, "each mesh is measured once");
        assert!(state.brick_resident(1) && state.brick_traced(1));
        assert!(!state.brick_resident(2) && !state.brick_resident(3));
        for _ in 0..32 {
            step(&mut state, &[a, b, c]);
        }
        assert_eq!(
            state.fit_walks(),
            walked,
            "a full pool must not walk a mesh it already refused"
        );
        state.set_gi_proxy_off([1]);
        step(&mut state, &[b, c]);
        assert_eq!(state.fit_walks(), walked, "the free reuses the cached sizes");
        assert!(!state.brick_resident(1));
        assert!(state.brick_resident(2) && state.brick_traced(2));
        assert!(!state.brick_resident(3), "one freed brick fits one waiting mesh");
        let walked = state.fit_walks();
        for _ in 0..8 {
            step(&mut state, &[b, c]);
        }
        assert_eq!(state.fit_walks(), walked);
        assert!(state.brick_traced(2));
        assert!(!state.brick_resident(3));
    }
    /// ADR-5506: an instance record carries its material's roughness and metallic factors (the probe
    /// rays' hit model), the roughness never 0 (0 in the shader means a hit without a material).
    #[test]
    fn an_instance_record_carries_the_materials_roughness_and_metallic() {
        let (vertices, indices, _) = thin_bar();
        let record = |roughness: f32, metallic: f32| {
            let material = crate::LabMaterial {
                roughness,
                metallic,
                ..crate::LabMaterial::default()
            };
            let mut state = DdgiState::new(pool_of(2, 30));
            step(&mut state, &[proxy(1, &vertices, &indices, &material)]);
            assert_eq!(state.instances.len(), 1);
            (state.instances[0].albedo[3], state.instances[0].emissive[3])
        };
        assert_eq!(record(0.6, 0.25), (0.6, 0.25));
        assert_eq!(record(0.0, 1.0), (HIT_ROUGHNESS_MIN, 1.0));
        assert_eq!(record(3.0, -2.0), (1.0, 0.0));
        assert_eq!(record(f32::NAN, f32::NAN), (1.0, 0.0));
    }

    /// The numbers after `marker` in the trace shader, up to the next `;`.
    fn shader_numbers(source: &str, marker: &str) -> Vec<f64> {
        let at = source
            .find(marker)
            .unwrap_or_else(|| panic!("`{marker}` is in ddgi_trace.wgsl"));
        let rest = &source[at + marker.len()..];
        let end = rest.find(';').expect("the statement ends");
        rest[..end]
            .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
            .filter(|t| t.contains('.'))
            .map(|t| t.parse().unwrap_or_else(|_| panic!("`{t}` is a number")))
            .collect()
    }

    /// ADR-5506: the trace shader's fit of the dielectric lobe's directional albedo (`hit_specular_albedo`)
    /// and of the diffuse lobe's Fresnel loss follow the forward pass's lobes (`pbr_brdf.wgsl`
    /// `isotropic_lobes`: GGX D, height-correlated Smith, Schlick at F0 0.04, Lambert under 1 − F(l·h)),
    /// integrated here under even light. The fit's rows are read from the shader's text.
    #[test]
    fn the_trace_shaders_specular_albedo_fit_follows_the_forward_lobe() {
        let source = include_str!("../shaders/ddgi_trace.wgsl");
        let rows: Vec<Vec<f64>> = ["let c0 = ", "let c1 = ", "let c2 = ", "let c3 = "]
            .iter()
            .map(|m| shader_numbers(source, m))
            .collect();
        assert!(
            rows.iter().all(|r| r.len() == 4),
            "four cubics of four terms: {rows:?}"
        );
        let loss = shader_numbers(source, "const DDGI_DIFFUSE_FRESNEL_LOSS: f32 = ")[0];
        let fit = |mu: f64, r: f64| {
            let x = 1.0 - mu;
            let c: Vec<f64> = rows
                .iter()
                .map(|row| row[0] + row[1] * r + row[2] * r * r + row[3] * r * r * r)
                .collect();
            ((c[3] * x + c[2]) * x + c[1]) * x + c[0]
        };
        // (specular albedo at F0 0.04, mean of (1 − l·h)^5) under a cosine-weighted hemisphere of light.
        let integral = |mu: f64, perceptual: f64| {
            let a = perceptual * perceptual;
            let a2 = a * a;
            let v = [(1.0 - mu * mu).max(0.0).sqrt(), 0.0, mu];
            let (n_u, n_phi) = (96usize, 192usize);
            let (mut spec, mut loss) = (0.0f64, 0.0f64);
            for i in 0..n_u {
                let u = (i as f64 + 0.5) / n_u as f64;
                let (ct, st) = ((1.0 - u).sqrt(), u.sqrt());
                for j in 0..n_phi {
                    let phi = std::f64::consts::TAU * (j as f64 + 0.5) / n_phi as f64;
                    let l = [st * phi.cos(), st * phi.sin(), ct];
                    let h = [l[0] + v[0], l[1] + v[1], l[2] + v[2]];
                    let len = (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt();
                    let no_h = (h[2] / len).clamp(0.0, 1.0);
                    let lo_h = ((l[0] * h[0] + l[1] * h[1] + l[2] * h[2]) / len).clamp(0.0, 1.0);
                    let no_l = ct.max(1e-6);
                    let fc = (1.0 - lo_h).powi(5);
                    let f = (no_h * a2 - no_h) * no_h + 1.0;
                    let d = a2 / (std::f64::consts::PI * f * f);
                    let vis = 0.5
                        / (no_l * (mu * mu * (1.0 - a2) + a2).sqrt()
                            + mu * (no_l * no_l * (1.0 - a2) + a2).sqrt());
                    spec += d * vis * (0.04 + 0.96 * fc);
                    loss += fc;
                }
            }
            let n = (n_u * n_phi) as f64;
            (std::f64::consts::PI * spec / n, loss / n)
        };
        let mut worst = (0.0f64, 0.0f64);
        for perceptual in [0.45, 0.6, 0.75, 0.9, 0.95, 1.0] {
            for mu in [0.05, 0.1, 0.2, 0.35, 0.5, 0.75, 1.0] {
                let (spec, fresnel) = integral(mu, perceptual);
                worst.0 = worst.0.max((fit(mu, perceptual) - spec).abs());
                worst.1 = worst.1.max((loss * (1.0 - mu).powf(2.5) - fresnel).abs());
            }
        }
        // Head-on a rough dielectric's lobe reflects about 1.4 % of even light, at a grazing angle 7 %.
        let (head_on, grazing) = (integral(1.0, 0.95).0, integral(0.05, 0.95).0);
        assert!(
            head_on > 0.012 && head_on < 0.017 && grazing > 0.06 && grazing < 0.08,
            "the lobe's albedo head-on {head_on}, grazing {grazing}"
        );
        assert!(
            worst.0 < 0.004 && worst.1 < 0.001,
            "the fits stray from the integrals by {worst:?}"
        );
    }

    /// ADR-5552: the 24 crease texels of a probe's tile are 24 different texels of its border ring, never
    /// the corner that holds the surface word, and the shader names the same texels (`ddgi_crease_texel` and
    /// its inverse `ddgi_crease_index`, read from the shader's text).
    #[test]
    fn the_crease_texels_are_the_border_ring_without_the_surface_words_corner() {
        let mut seen = std::collections::BTreeSet::new();
        for face in 0..6 {
            for side in 0..4 {
                let (x, y) = crease_texel(face, side);
                assert!(
                    x < 8 && y < 8 && (x == 0 || x == 7 || y == 0 || y == 7),
                    "crease {face}/{side} lies on the border ring: ({x}, {y})"
                );
                assert_ne!((x, y), (0, 0), "the corner holds the surface word");
                assert!(seen.insert((x, y)), "crease {face}/{side} has a texel of its own");
            }
        }
        assert_eq!(seen.len(), 24);
        // The shader's forward map and its inverse, line for line.
        let source = include_str!("../shaders/ddgi_common.wgsl");
        for line in [
            "return vec2<u32>(i + 1u, 0u);",
            "return vec2<u32>(i - 7u, 7u);",
            "return vec2<u32>(0u, i - 14u);",
            "return vec2<u32>(7u, i - 20u);",
            "return t.x - 1u;",
            "return 7u + t.x;",
            "return 14u + t.y;",
            "return 20u + t.y;",
        ] {
            assert!(source.contains(line), "ddgi_common.wgsl holds `{line}`");
        }
        // The breaks of the forward map (crease 7, 15 and 21 start a new side of the ring).
        assert_eq!(crease_texel(1, 2), (7, 0));
        assert_eq!(crease_texel(1, 3), (0, 7));
        assert_eq!(crease_texel(3, 2), (7, 7));
        assert_eq!(crease_texel(3, 3), (0, 1));
        assert_eq!(crease_texel(5, 0), (0, 6));
        assert_eq!(crease_texel(5, 1), (7, 1));
        assert_eq!(crease_texel(5, 3), (7, 3));
    }

    /// ADR-5796: the screen lookup builds a texel's normal from the pixels around its own, and a neighbor
    /// outside the frame is a zero difference by construction (a `select` on the pixel's coordinates). It used
    /// to be the pixel itself, read through a clamp and subtracted from itself, which is 0 only where two
    /// evaluations of one expression round alike: on an NVIDIA card the lookup's first row and column held
    /// normals that pointed anywhere (`tests/dynamic_gi.rs`
    /// `the_frames_first_row_and_column_read_the_probes_as_the_rows_beside_them` is the frame's side of this).
    /// The pair of neighbors the normal takes is also tested against the diagonal pixel between them, so a
    /// pair that straddles a crease is not used.
    #[test]
    fn the_screen_lookup_takes_no_difference_from_a_pixel_outside_the_frame() {
        let source = include_str!("../shaders/ddgi_screen.wgsl");
        for line in [
            "let r = select(zero, world_at(px + vec2<i32>(1, 0)).xyz - p, px.x + 1 < full.x);",
            "let l = select(zero, p - world_at(px - vec2<i32>(1, 0)).xyz, px.x > 0);",
            "let u = select(zero, world_at(px + vec2<i32>(0, 1)).xyz - p, px.y + 1 < full.y);",
            "let b = select(zero, p - world_at(px - vec2<i32>(0, 1)).xyz, px.y > 0);",
        ] {
            assert!(source.contains(line), "ddgi_screen.wgsl holds `{line}`");
        }
        assert_eq!(
            source.matches("world_at(px - vec2<i32>(").count(),
            2,
            "the two pixels before a texel's own are read once each, under their guard"
        );
        assert!(
            source.contains("let n = screen_normal(px, p, v);"),
            "the lookup takes `screen_normal`'s normal"
        );
        assert!(
            source.contains("if (off <= SCREEN_PLANAR) {"),
            "a texel whose four neighbors lie in one plane keeps `ddgi_depth_normal`'s normal and reads no \
             more depth"
        );
    }
}
