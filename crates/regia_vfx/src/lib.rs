// SPDX-License-Identifier: MPL-2.0
//! GPU particle VFX, projected decals, cosmetic debris events and camera shake.
//!
//! Specification: `docs/specs/26-terrain-vfx-highend.md` § 3 (VFX, decals, destruction) and the
//! DO27 event set of `docs/specs/25-sim-bridge-and-do27.md` § 4. Physics-backed debris lives in
//! `regia_physics` (cosmetic-only subset, `docs/specs/15-physics.md` § 1); this crate spawns
//! everything else.
//!
//! - [`format`](mod@format): the frozen `.vfx.ron` v6 asset (`docs/formats/vfx-ron.md`). Older versions
//!   migrate on load.
//! - [`sim`]: CPU spawn scheduling — deterministic counts, budgets, LOD.
//! - [`gpu`]: compute simulation (ring pool, curl noise, depth collision), bitonic sort,
//!   billboard / stretched / mesh-particle rendering, projected decals, trails. The
//!   [`gpu::VfxGpu`] type is the integration hook for render hosts.
//! - [`offscreen`]: [`offscreen::VfxHarness`] — the reference host: a lit ground + sky scene
//!   rendered with VFX on top, offscreen, for evidence and GPU tests.
//! - [`measure`]: with/without image-diff metrics (coverage, luminance, histogram) and the
//!   not-empty/not-black/not-blown-out checks used by the M14 evidence tests.
//! - [`runtime`]: [`runtime::VfxRuntimePlugin`] — wires the ECS runtime path
//!   (extract → `FrameRenderer`) up to a [`gpu::VfxGpu`] through `regia_render`'s
//!   `TranslucentLayer` hook.
//! - [`scene`]: the ECS-facing [`scene::VfxScene`] + [`plugin::VfxPlugin`] that tie it together.
//! - [`entry`]: atmospheric entry streak (bolide head, ionised trail, dust train, shed pieces, dark flight).
//! - [`shake`]: trauma-decayed camera shake driven by [`shake::CameraShakeRequest`] messages.
//! - [`showcase`]: the DO27 reference effect set (also used by `samples/vfx_lab` and the
//!   offscreen evidence suite), with atmosphere-aware explosion and smoke variants.
//! - [`medium`]: the level's air for effects ([`medium::VfxMedium`]: airless, Mars, Earth, and
//!   gravity); the scene plays `name@airless` / `name@mars` variants by default.
//! - [`hover`]: hover-unit ground interaction ([`hover::HoverWash`]): dust, ice and frost kicked
//!   up under hulls, scaled by height and footprint, atmosphere-aware.
//! - [`plume`]: engine and thruster plumes ([`plume::ThrusterPlume`]) per atmosphere: shimmer and
//!   a faint exhaust in air, a thin plume on Mars, only the nozzle glow (lighting the ground when
//!   low) airless; keyed to DO27's ducts and jetpacks.
//! - [`combat`]: weapon hits on hulls, buildings and glass ([`combat::WeaponHit`], feeding
//!   glass-damage), muzzle flashes and tracers that read in daylight, keyed to DO27's weapons.
//! - [`crash`]: crash and impact effects that scale from a vehicle crash to a large surface
//!   explosion ([`crash::CrashScale`]): fireball and flash, condensation shell, blast ring, base
//!   surge, ejecta, mushroom column, ground fires and a first-contact puff, on a clock with
//!   crater and blast-arrival hooks ([`crash::CrashSequence`]), sized and timed from public
//!   physical scaling, with a cheap tier for Low, Deck and mobile ([`crash::CrashTier`]).
//! - [`ambient`]: ambient life (ADR-3024, opt-in): motes in the sun, insect swarms, butterflies,
//!   fireflies, falling leaves and bird flocks whose positions the audio's bird calls follow.
//! - [`probe`]: a CPU kinematic probe that plays an effect's real spawn jobs through the GPU
//!   kernel's force model, so sizes, speeds and timings can be measured without a GPU.
//!
//! ## Render integration hook
//!
//! 1. Own a [`gpu::VfxGpu`] created with [`gpu::VfxProfile`] (no format: render pipelines are
//!    built lazily per colour format).
//! 2. Once per frame, after opaque geometry is encoded but before presenting: call
//!    [`gpu::VfxGpu::set_depth_view`] (the scene depth), [`gpu::VfxGpu::set_frame`] and
//!    [`gpu::VfxGpu::simulate`] on the frame's encoder.
//! 3. Draw into a transparent pass (depth attachment = the same `Depth32Float` view,
//!    **read-only** — the shaders sample it): [`gpu::VfxGpu::draw_decals`], then
//!    [`gpu::VfxGpu::draw_particles`], then [`gpu::VfxGpu::draw_trails`]. On a single-sample
//!    `Rgba16Float` frame the runtime hook draws into that HDR target before bloom
//!    ([`gpu::VfxGpu::set_exposure`] divides the authored colour by the post exposure).
//!    MSAA and `Rg11b10` frames draw into the 8-bit display target instead (ADR-049).
//!
//! # How the pieces fit
//!
//! The ECS-facing surface is [`plugin::VfxPlugin`] + [`scene::VfxScene`]: gameplay sends
//! [`SpawnEffect`] requests (and [`SpawnDecal`] / [`CameraShakeRequest`]), [`sim`] schedules
//! deterministic counts, and [`runtime::VfxRuntimePlugin`] extracts them into a [`gpu::VfxGpu`]
//! that simulates and draws through the render hook. [`format`](mod@format) is the frozen `.vfx.ron` asset,
//! [`shake`] decays trauma into camera shake, and [`showcase`] / [`offscreen`] are the reference
//! effect set and its offscreen host.
//!
//! See `book/user-guide/terrain-vfx.md` (particles, decals).

pub mod ambient;
pub mod atlas;
pub mod atmos_presets;
pub mod combat;
pub mod crash;
pub mod curves;
pub mod debris_mesh;
pub mod decal;
pub mod decal_atlas;
pub mod decal_cull;
pub mod decal_format;
pub mod decal_scene;
pub mod decal_strip;
pub mod entry;
pub mod entry_runtime;
pub mod events;
pub mod evidence;
pub mod fire_light;
pub mod format;
pub mod glass;
pub mod gpu;
pub mod hover;
pub mod impact_site;
pub mod kill_volume;
pub mod lights;
pub mod live_state;
pub mod measure;
pub mod medium;
pub mod mesh_shape;
pub mod offscreen;
pub mod plugin;
pub mod plume;
pub mod precip;
pub mod probe;
pub mod quality;
pub mod ribbon;
pub mod runtime;
pub mod scene;
pub mod shake;
pub mod showcase;
pub mod significance;
pub mod sim;
pub mod timing;
pub mod trails;
pub mod vehicle_fx;
pub mod weather;

pub use ambient::{
    AmbientBirds, AmbientLifeHandle, AmbientLifePlugin, AmbientLifeReport, AmbientLifeSettings, BirdMode,
    FollowAmbientBird, Season,
};
pub use combat::{Calibre, HitSurface, WeaponHit, apply_weapon_hit};
pub use debris_mesh::{
    DebrisBursts, DebrisClass, DebrisClock, DebrisMesh, DebrisMeshLibrary, DebrisPlugin, DebrisSpec,
    fragment_budget,
};
pub use decal::{
    CHANNEL_ALBEDO, CHANNEL_EMISSIVE, CHANNEL_NORMAL, CHANNEL_ROUGHNESS, Decal, DecalAnchor,
    DecalDrawScratch, DecalPool, DecalSurface, HiddenStaticDecals, LAYER_CHARACTER, LAYER_STATIC_MESH,
    LAYER_TERRAIN, LAYER_WORLD, PlaceDecal, SpawnDecal, accepts_layer, align_along, align_to_normal,
    angle_fade, distance_fade, place_decal,
};
pub use decal_atlas::{DECAL_ATLAS_LAYERS, DECAL_ATLAS_SIZE, generate_decal_atlas};
pub use decal_cull::{DECAL_TILE_PX, DecalBin, DecalCullStats, MAX_TILES, bin_decals};
pub use decal_format::{
    DECAL_FORMAT_VERSION, DecalFormatError, DecalLibrary, DecalPrefab, StaticDecal, atlas_layer,
    builtin_prefab, decal_migration_sources, decal_schema_fingerprint, world_mask_excludes_characters,
};
pub use decal_scene::{DecalFrame, DecalScene, measure_decal_pass, project_pixel};
pub use decal_strip::{DecalStrip, sample_quadratic};
pub use entry_runtime::{EntryStreak, EntryStreakPlugin};
pub use fire_light::{CrashFires, FireLight, FireLightInstance, FireLightSample, FireLights};
pub use format::{
    EventSpawnDef, EventTrigger, VFX_FORMAT_VERSION, VfxFormatError, VfxSystemDef, vfx_migration_sources,
    vfx_schema_fingerprint,
};
pub use gpu::events::PoolParticle;
pub use gpu::{EmitterGpu, VfxCamera, VfxError, VfxGpu, VfxProfile, VfxWarmReport, vfx_warm_job};
pub use hover::{HoverSurface, HoverWash, hover_intensity};
pub use impact_site::{DamageSwap, ImpactDetail, ImpactSitePlugin, ImpactSiteSpec, ImpactSites};
pub use measure::{EffectMetrics, assert_effect_plausible, measure_effect};
pub use medium::{VfxAtmosphere, VfxMedium};
pub use plugin::{SpawnEffect, VfxLook, VfxPlugin, VfxSettings, VfxView};
pub use plume::{PlumeKind, ThrusterPlume};
pub use precip::PrecipResolution;
pub use precip::{
    DropSample, PrecipBudget, PrecipCounts, PrecipFrame, PrecipIntegrator, PrecipitationGpu,
    PrecipitationHandle, PrecipitationReport, PrecipitationSettings, SplashSample,
};
pub use runtime::{
    PARTICLE_LIGHT_LUMENS_PER_UNIT, VfxRuntimePlugin, VfxTrails, particle_point_lights,
    particle_point_lights_scaled, push_particle_lights, translucent_layer, translucent_layer_with_dt,
};
pub use scene::VfxScene;
pub use shake::{CameraShake, CameraShakeRequest};
pub use showcase::showcase_effects;
pub use timing::GpuTimer;
pub use trails::{TrailRig, TrailStyle};
pub use weather::{
    CloudLookOverride, PrecipitationKind, PrecipitationRates, WEATHER_PRESETS, Weather, WeatherClock,
    WeatherConfig, WeatherConfigError, WeatherPlugin, WeatherState, drive_cloud_weather, level_clouds,
};

/// Rust and WGSL struct rows of the particle harness and precipitation shaders: the host type of each
/// uniform struct, built beside the private types (`regia_render::shader_contract`, ADR-3002).
#[must_use]
pub fn shader_contract_rows() -> Vec<regia_render::ContractRow> {
    let mut rows = offscreen::contract_rows();
    rows.extend(precip::contract_rows());
    rows.extend(ambient::contract_rows());
    rows
}
// Forward decals: which path shades each decal and the forward pass's copy (ADR-3255).
pub mod decal_forward;
pub use decal::{CHANNEL_METALLIC, DecalShading};
pub use decal_atlas::{DECAL_ATLAS_PAGES, generate_decal_atlas_pages};
pub use decal_forward::{DecalPath, DecalRoute, forward_atlas, forward_copy, route_decals};
// Effect bounds: fixed (authored), measured (CPU reference with extents) and GPU-reduced (P-225, ADR-8355).
pub mod bounds;

pub mod damage_atlas;
/// Custom image preparation shared by the projected and forward decal paths.
pub mod decal_texture;
pub use damage_atlas::{
    DAMAGE_DECAL_ATLAS_SIZE, DAMAGE_DECAL_CELL, DAMAGE_DECAL_CELLS, damage_decal_atlas_fnv1a64,
    generate_damage_decal_atlas,
};
// Decal v3 integration (ADR-8829): damage-atlas cells as custom images, footstep surfaces.
pub use damage_atlas::{damage_decal_cell_image, damage_decal_cell_key};
pub use decal::DECAL_FOOTSTEP_NAMES_MAX;
pub use decal_format::{DECAL_SURFACE_NAME_MAX, validate_surface_name};
