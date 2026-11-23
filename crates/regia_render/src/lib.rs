// SPDX-License-Identifier: MPL-2.0
//! wgpu renderer: render graph, materials, lighting, shadows, post-processing.
//!
//! Specification: `docs/specs/13-renderer.md`. M1 brings up the device and per-window surfaces with
//! the Android lifecycle contract (surfaces dropped on suspend, recreated on resume, device kept) and
//! clears each window to [`ClearColor`]. M3 adds PBR forward shading, cascaded shadow maps, and
//! AgX tonemapping.
//!
//! # How the pieces fit
//!
//! The ECS surface is small and what games touch: [`Camera`] + a `GlobalTransform` makes a view,
//! [`MeshRenderer`] attaches a shared [`CpuMesh`]/[`CpuMaterial`] to an entity, and
//! [`DirectionalLight`]/[`PointLight`]/[`SpotLight`] light the scene. The [`Environment`] resource
//! sets ambient/IBL, and [`RenderSettings`] carries the active [`RenderProfile`] (quality presets).
//! Each frame the runtime path ([`runtime`]) extracts those components into an
//! [`ExtractedFrame`] and renders the pass sequence — shadows → clusters → prepass → forward →
//! IBL → post → [`gizmos`] — into window surfaces and offscreen targets. The device, surfaces and clear pass
//! live in [`RenderPlugin`]; everything below is implementation detail that games rarely touch.
//!
//! See `book/user-guide/rendering.md` (profiles, exposure, capture) and
//! `book/user-guide/materials.md` (materials and textures).

mod atmosphere;
pub mod baked_gi;
mod batch;
mod bind_cache;
pub mod boot;
pub mod breadcrumbs;
pub mod calibration;
mod camera;
pub mod camera_cut;
pub mod capabilities;
mod cascade_anchor;
mod clouds;
pub mod compare;
pub mod compressed_texture;
pub mod contact_shadows;
pub mod cooked_model;
mod ddgi;
mod ddgi_bake;
mod ddgi_gpu;
mod debug;
pub mod debug_surface;
pub mod debug_view;
mod decal_receiver;
mod depth_pyramid;
mod device;
pub mod device_pipelines;
mod display_stage;
mod dof;
mod dynamic_res;
pub mod emissive_lights;
pub mod exposure_zones;
pub mod fade_overlay;
mod mips;
mod outdoor;
mod platform;
pub mod surface_weather;
mod volumetrics;
pub use device::{
    baseline_optional_features, create_window_instance, instance_backends, instance_creation_count,
    loader_list_lock_count, request_device_locked,
};
pub mod fog;
pub mod forward;
mod frame;
pub mod frame_gen;
mod frame_stats;
pub mod gizmos;
pub mod gpu_health;
pub mod gpu_select;
mod texture_residency;
pub use frame_stats::KeyAxes;
pub use frame_stats::{BLOCKING_POLL_SITES, BlockingPollSite, BlockingPollSites, blocking_poll_sites};
pub use frame_stats::{
    CREATE_LABEL_BYTES, CreateDetail, CreateRecord, FramePathCounters, FramePathEvent, FramePathKind,
    FrameWaits, RECENT_CREATES, TextureViewStats, UploadSplit, WindowPhases, counters as frame_path_counters,
    create_buffer, create_seq, frame_waits, process_counters as frame_path_counters_all_threads,
    recent_creates, take_events as take_frame_path_events, texture_view_stats, trace as trace_frame_path,
    upload_split, window_phases, with_texture_view_stats,
};
pub use frame_stats::{log_pipeline_creates, take_pipeline_creates};
pub mod hitch;
pub use gpu_health::{DEVICE_LOST_EXIT_CODE, DeviceLostInfo, SURFACE_UNAVAILABLE_EXIT_CODE, device_lost};
pub use gpu_select::{AdapterSelection, AdapterSummary, GpuRequest, adapter_for_surface, list_adapters};
pub use hitch::{HitchFrame, HitchRecorder, HitchReport, HitchSpike, HitchTargets};
mod gi;
pub mod glass;
mod gpu_driven;
mod graph;
mod graph_pipelines;
pub use graph_pipelines::{GraphPipelineStats, GraphTarget};
mod exit;
mod frame_pipelining;
mod fsr3;
pub mod hair;
mod handle_args;
pub mod hard_surface;
mod headless;
mod highend;
pub mod host_checklist;
pub mod host_memory;
mod ibl;
pub mod immediate;
mod interior;
mod key_hash;
mod lab_frame;
mod lens;
mod light_budget;
mod lights;
mod local_exposure;
pub mod low_memory;
pub mod ltc;
pub mod lut;
pub mod mask_decals;
pub mod masked_occlusion;
pub mod material;
pub mod memory;
pub mod memory_budget;
mod mesh_clean;
mod mesh_lod;
pub mod mesh_occluders;
mod mesh_storage;
pub mod minspec;
pub mod mist;
pub mod morph;
mod motion_blur;
mod offscreen;
pub mod oit;
pub mod output;
pub mod overlay;
mod paged_shadow;
mod parallel_finish;
pub mod pipeline_cache;
pub mod render_out;
pub mod render_override;
pub mod shading_model;
pub mod subsurface;
pub mod thin_film;
pub use exit::{GpuExitReport, LiveGpuObjects, last_gpu_exit_report};
pub mod pipeline_warm;
mod post;
mod post_pass;
pub mod post_ref;
pub mod prepare;
pub mod primitives;
pub mod probe_specular;
mod profile;
pub mod quality;
mod render_thread;
pub use quality::{
    AdapterProfile, Detection, GpuTierTable, ProbeConfig, ProbeResult, ProfileName, QualityProbe, QualityTier,
};
mod readback;
mod reflections;
pub mod runtime;
pub mod scalar_overlay;
mod scalar_overlay_pass;
pub mod scene_export;
pub mod screenshot;
mod sdf;
mod shader_check;
pub mod shader_hlsl;
#[cfg(test)]
mod shader_hlsl_check;
mod shader_lib;
pub mod shader_reload;
#[cfg(test)]
mod shader_sources;
pub mod shadow;
mod shared_draw;
mod shared_lights;
mod skin;
mod skin_compute;
pub mod skinning;
mod smaa;
mod soft_math;
pub mod specular_aa;
mod ssao;
mod sssr;
mod staging;
mod surface;
pub mod surface_graph;
mod taau;
mod tangents;
pub mod texture_budget;
pub mod texture_stream;
mod texture_view;
pub use texture_view::TextureViewSettings;
pub mod backend_policy;
pub mod bindless;
pub mod gi_denoise;
pub mod hwrt;
pub mod lightmap;
pub mod lightmap_debug;
mod stream_hooks;
pub use runtime::MeshShadowCast;
pub use stream_hooks::StreamRenderResidency;
pub use stream_hooks::{HlodProxyShadows, hlod_proxy_shadow_cast, hlod_voxel_mesh};
mod submission;
pub mod sun_cascades;
pub mod taau_quality;
pub mod tonemap;
pub mod translucent;
mod tsr;
pub mod upload_queue;
pub mod upscaler;
pub mod view_math;
pub mod visbuffer;
pub use bindless::{BindlessCaps, BindlessTier, MaterialArenaStats};
pub mod texture_table;
pub use frame_stats::{COUNTED_BIND_GROUPS, FrameDrawCounts, PassDrawCounts};
pub use frame_stats::{DRAW_RECORD_PASSES, DrawLogCounters, DrawRecord, draw_log_counters};
pub use texture_table::{MaterialSampler, MaterialSamplers, TableFallback, TableSupport, TextureTableStats};
// Bindless step 2a (ADR-356): texture-set keys and the census.
mod exr_out;
pub mod resource_inspector;
mod texture_sets;
pub use texture_sets::TextureSetStats;
mod material_clock;
pub mod temporal_stability;
// Bindless step 5 (ADR-1110): the texture table's growth policy and GPU residency feedback.
pub mod residency_feedback;
pub mod table_heap;
pub use residency_feedback::{FeedbackMode, MaterialResidency, ResidencyFeedbackStats};
// Bounded upload staging during level loads (ADR-1346).
pub mod staging_flush;
pub use staging_flush::{StagingFlushStats, set_staging_flush_cap, staging_flush_stats};

// Swapchain plan and report: DX12 present and HDR rules (ADR-525).
pub mod swapchain;
// Light source size (ADR-1367): the optional component and its CPU mirrors.
pub mod light_source;
pub use light_profiles::LightProfileLibrary;
pub use lights::LightSourceSize;
pub use lights::{LightProfile, LightSourceLtc, RectLight, TubeLight};
// Fog local lights (ADR-1365): candela units, importance pick, spot cones.
pub mod fog_lights;
pub use fog_lights::{
    FOG_LIGHT_SLOTS, FOG_PICK_HYSTERESIS, FogLightCandidate, FogLightChoice, FogLightPick, fog_light_score,
    pick_fog_lights,
};
// Cinematic offline profile (ADR-1403): the cloud march knobs, the tier gate, preset choice.
pub mod cinematic;
pub use cinematic::CloudMarch;
// Deep (16-bit) offscreen readout for render-out (ADR-1404).
pub mod deep_readout;
pub use deep_readout::{DeepFrame, DeepTarget};
// Cloud genera, the diffuse cloud light and the cirrus sheet (ADR-1408).
pub mod cloud_types;
pub use cloud_types::{
    CIRRUS_ICE_ASYMMETRY, CIRRUS_WIND_RATIO, CLOUD_DIFFUSION_PHYSICAL, CLOUD_DROPLET_ASYMMETRY, CirrusSheet,
    CloudGenus, CloudGenusLayer, CloudLook, cirrus_direction_row, diffuse_transmission,
};
// The cirrus sheet's shading, CPU mirror (ADR-1425).
pub use cloud_types::{CIRRUS_AIR_SCALE_HEIGHT_M, CirrusView, cirrus_composite, cirrus_phase, cirrus_ridge};
// The haze's Ångström colour for a low sun (ADR-1408).
pub mod aerosol;
pub use aerosol::{AEROSOL_WAVELENGTHS_NM, AerosolSpectrum, CONTINENTAL_ANGSTROM, angstrom_factors};
// Daylight calibration: the grey card under the engine's sun and sky (ADR-1425).
pub mod daylight;
pub use daylight::{GREY_CARD_ALBEDO, GreyCard, ies_clear_sky, metered_card};
// Post volumes and physical camera exposure (ADR-1464).
pub mod post_volumes;
// Per-vertex ambient occlusion on opaque lab materials (ADR-1512).
pub mod vertex_ao;
pub use dof::vertical_fov_from_focal_length;
pub use lens::CameraExposureMode;
pub use post_volumes::{EyeRange, PostOverrides, PostVolume, PostVolumeShape};
// The forward shader's closed feature axes and composed permutations (ADR-1552).
pub mod forward_permutations;
pub use forward_permutations::{
    FORWARD_AXES, FORWARD_PERMUTATION_BUDGET, ForwardAxis, ForwardPermutationReport,
    forward_permutation_report, graph_permutation_defines,
};
// Shader-permutation policy: the cooked graph pipeline list per profile, its budget and the
// runtime guard (master plan § 5.5a, ADR-2026).
pub mod permutation_policy;
pub use permutation_policy::{
    PermutationAdmission, PermutationBudget, PermutationEnforcement, PermutationGuard, PermutationGuardStats,
    PermutationManifest, PermutationProfileCount,
};
// Gradient and HDR-image studio environments for the IBL (ADR-1653).
mod env_ground;
mod local_capture;
mod studio_env;
// Foliage wind and the dynamic-caster shadow class (`specs/88`, ADR-2028).
mod foliage_wind;
pub mod wind_natural;
pub use foliage_wind::{
    DynamicCasterPolicy, DynamicCasterReport, DynamicCasterShadows, FOLIAGE_SWAY_LIMIT_SLOT,
    FOLIAGE_SWAY_SLOT, FOLIAGE_SWAY_VERTEX, FOLIAGE_WIND_MAX_M_PER_S, FoliageSway, sanitize_wind,
};
pub use wind_natural::WindStyle;
// Dynamic GI quality tiers over the SDF-traced probes (specs/79, ADR-1679).
pub mod dynamic_gi;
pub use dynamic_gi::{DdgiPyramidStats, DynamicGiBudget, DynamicGiStatus, DynamicGiTier};
pub mod radiance_cache;
pub use radiance_cache::RadianceCacheTerm;
pub use radiance_cache::RadianceCacheWork;
pub use radiance_cache::{RADIANCE_CACHE_MAX_LEVELS, RadianceCacheSettings, RadianceCacheStatus};
// Driver workarounds in shader composition (Adreno sun shadow, ADR-1749).
pub mod shader_workarounds;
// The Mobile tiers' lite forward path (ADR-1874).
pub mod forward_lite;
// Hashed alpha thresholds without a lattice, and the stipple measure (ADR-1489).
pub mod alpha_hash;
// Two-phase occlusion of the forward depth prepass (specs/98, ADR-1887).
mod prepass_occlusion;
pub use prepass_occlusion::{
    OCCLUSION_VIEW_RETIRE_ROUNDS, OcclusionViewPyramid, PrepassOcclusionCounters, PrepassOcclusionSkip,
    PrepassOcclusionStats,
};

use regia_app::prelude::*;

pub use atmosphere::{
    AERIAL_CLOUD_STEPS, CloudAirLight, ground_light, sample_aerial_sun, sample_aerial_under_clouds,
};
pub use atmosphere::{
    AtmosphereLuts, AtmosphereParameters, AtmospherePreset, LutImage, SkyBody, SunPosition, TimeOfDay,
    ambient_irradiance, ambient_irradiance_table, analytic_sky, build_atmosphere_luts,
    build_atmosphere_luts_for, display_radiance, sample_aerial, sample_sky, sample_sky_table,
    sun_colour_and_lux, sun_disk_radiance, sun_position, transmitted_sun, transmitted_sun_at,
};
pub use baked_gi::{
    BakedGiFrameLights, BakedGiSettings, BakedGiStatus, StaticGiSample, component_scales, sample_static_gi,
};
pub use baked_gi::{SunRelight, sample_static_gi_relit, sun_relight};
pub use batch::{
    BatchKey, DrawItem, InstancedBatch, batch_instanced, draw_call_count, sort_opaque, sort_transparent,
};
pub use boot::{BootBackground, BootMark, BootSplash, SPLASH_MAX_DURATION, SplashImage, SplashPhase};
pub use calibration::{DisplayCalibration, DisplayLuminance, DisplayModel, DisplayPlatform};
pub use camera::{Camera, ClearMode, RenderTarget, RenderView};
pub use camera_cut::{CameraCutDetection, CameraCutReason, CutExposure};
pub use capabilities::{RenderCapabilities, ResourceBindingTier, TERRAIN_SHARED_BIND_GROUPS};
pub use clouds::{
    CLOUD_BIG_STEP_RATIO, CLOUD_BLOCK, CLOUD_EMPTY_STEPS, CLOUD_FADE_M, CLOUD_FADE_MULTIPLE,
    CLOUD_FAR_STEP_FLOOR_M, CLOUD_FRESH_WEIGHT, CLOUD_NEAR_STEP_M, CLOUD_SKY_DISTANCE_M, CloudPattern,
    CloudSettings, WeatherMap, build_weather_map, cloud_bayer_offset, cloud_fresh_extent, cloud_fresh_texel,
    cloud_shadow_transmittance, cloud_small_step, column_optical_depth, height_gradient, measured_coverage,
    shell_intersections,
};
pub use clouds::{
    CLOUD_FRAME_SAMPLE_BUDGET, CLOUD_WEATHER_CUT_COVERAGE, CLOUD_WEATHER_CUT_DENSITY_RATIO,
    CLOUD_WEATHER_CUT_TYPE, CloudFrameWork, CloudWeatherSample, CloudWorkCounters, WeatherField,
    cloud_steady_work, cloud_weather_cut, cloud_weather_motion, cloud_work_counters,
};
pub use clouds::{
    CLOUD_OVERCAST_DIM, CLOUD_SHADOW_EDGE_FADE, CLOUD_SHADOW_FLOOR, CLOUD_SHADOW_MAP_EXTENT_M,
    CLOUD_SHADOW_MAP_SIZE, CLOUD_SHADOW_MAP_STEPS, CLOUD_SHADOW_MIN_SUN_Y, CLOUD_SHADOW_REFRESH_FRAMES,
    cloud_overcast_factor, cloud_shadow_cell, cloud_shadow_crossing, cloud_shadow_lookup, cloud_shadow_march,
    cloud_shadow_params, cloud_shadow_refresh_rects, cloud_shadow_shade, cloud_shadow_shade_for_depth,
    cloud_shadow_texel, cloud_shadow_texel_cell, cloud_shadow_window, overcast_sky,
};
// The air under the layer (ADR-5944).
pub use clouds::{CloudAirSpan, HAZE_CLOUD_STEPS, HAZE_SKY_FLOOR, cloud_air_span, cloud_shadow_on_ray};
// The column fill and the height profile, for the path tracer's cloud layer (ADR-1834).
pub use clouds::{COLUMN_FILL, type_gradient};
// The density's inputs and its bound, for the path tracer's cloud field (ADR-2127).
pub use clouds::{
    CLOUD_CURL_STRENGTH, CLOUD_CURL_TILE, CLOUD_MARCH_ALBEDO, CURL_SIZE, CloudNoise, SHAPE_NOISE_SIZE,
    cloud_curl, cloud_density_bound, cloud_noise,
};
pub use compressed_texture::{CompressedMaps, CompressedTexture, CompressedTextureError, NormalLayout};
pub use ddgi::GiLookupResolution;
pub use ddgi::{
    DDGI_MAX_UPDATE_DIVISOR, DDGI_UNIFORM_BYTES, DdgiReadback, DdgiScreenTexel, DdgiSettings,
    HeightfieldInfo, SDF_EVICT_FRAMES, SDF_POOL_BYTES, SDF_POOL_VALUES,
};
pub use ddgi::{FINEST_HEADROOM_M, MAX_SEED_UPDATES, SCREEN_LOOKUP_MAX};
pub use ddgi::{HEIGHT_EDIT_RECTS, HeightEdits};
pub use ddgi_gpu::ddgi_shading_wgsl;
pub use debug::{DebugDraw, DebugLine, DebugText, DebugViewMode};
pub use debug_surface::{
    DebugLegendSink, DebugSurfaceFrame, DensityClass, LegendEntry, MaterialLabel, debug_surface_meshes,
    material_color, texel_density_class, triangle_texel_density,
};
pub use debug_view::{DebugView, wireframe_batches, wireframe_frame};
#[cfg(test)]
pub(crate) use device::test_device;
pub use device::{RenderDevice, TrackedDevice, pipeline_timing_summary};
pub use display_stage::HdrTestPattern;
pub use dof::{DepthOfField, DepthOfFieldMode, SUPER_35_SENSOR_HEIGHT, focal_length};
pub use dynamic_res::{
    CPU_BOUND_GPU_SHARE, DynResConfig, DynResController, DynResGovernor, FrameBound, GPU_BOUND_GPU_SHARE,
    classify_bound,
};
pub use env_ground::{
    EnvGroundError, EnvGroundPlane, PlaneCover, cap_form_factor, plane_cover, reflection_share,
};
pub use exposure_zones::{
    ADAPT_DT_CAP_SECONDS, DEFAULT_ADAPT_DOWN_SPEED, DEFAULT_ADAPT_UP_SPEED, EXPOSURE_LOG_DEFAULT_CAP,
    EvRange, EvTarget, ExposureLog, ExposureSample, ExposureZone, ExposureZoneShape,
    MAX_TUNNEL_FRAME_DELTA_EV, adapt_ev, combine_zones, zone_signed_distance, zone_weight,
};
pub use fade_overlay::{FADE_LAYER, FADE_ORDER, FadeHandle, FadeState, fade_output, install_fade};
pub use fog::{
    FOG_CASCADE_FADE, FOG_LEGACY_SHADOW_BIAS_NDC, FOG_SHADOW_BIAS_TEXELS, FOG_SHADOW_GROUND_SLOPE_TEXELS,
    FOG_SHADOW_MAX_SLOPE, FogCascadeState, FogSunCascade, FogSunShadowFrame, FogSunShadowProbe,
    FogSunShadowRule, FogSunShadowSample, fog_cascade_fade, fog_sample_depth, fog_sample_weight,
    fog_sun_shadow_bias_m,
};
pub use fog::{FogSettings, apply_fog, fog_factor};
pub use forward::{CASCADE_COUNT, DirectionalLightUniform, ForwardResult, ShadowUniform, ShadowUniforms};
pub use frame::{
    CANOPY_EYE_MARGIN_M, CacheBudgets, CacheResidency, CacheStats, CanopyDraws, DeviceFrameRenderer,
    DrawIntegrity, DrawStats, EPHEMERAL_KEY, FrameCpuTimings, FrameExtent, FrameMesh, FrameRenderer,
    FrameScene, FrameScratchStats, FrameTarget, Frustum, GlassPipelineTarget, GpuNodePass, GpuStamp,
    GpuStampNodes, GpuTimings, HdrCapture, LOD_DITHER_NONE, MeshBounds, MotionField, OffscreenTarget,
    RendererDropReport, SHADOW_LOD_MESH_TAG, ScreenPassReport, ShadowCacheReport, ShadowCast,
    ShadowCpuTimings, TextureViewFrame, ZERO_KEY_WARN_MIN, render_frame_into, signal_memory_pressure,
};
// CPU masked occlusion of the main view's draw list (ADR-10072).
pub use frame::{
    EyeCamera, EyeFrame, EyeFrameReport, EyeHiddenArea, STEREO_EYE_VIEWS, hidden_area_clip_vertices,
    read_rgba8, stereo_eye_of_view,
};
pub use frame::{
    MASKED_CULL_HEIGHT, MASKED_CULL_WIDTH, MASKED_IDLE_FRAMES, MASKED_MAX_OCCLUDERS, MASKED_MIN_CULLABLE,
    MASKED_OCCLUDER_MIN_SCREEN, MASKED_OCCLUDER_TRIANGLE_BUDGET, MASKED_PROBE_INTERVAL, MaskedCullReport,
    MaskedCullSkip,
};
pub use frame_pipelining::FramePipelining;
pub use fsr3::Fsr3Settings;
pub use fsr3::{FSR3_REFERENCE, FSR3_REFERENCE_DIFFERENCES, Fsr3ReferenceDifference};
pub use gi::{
    GiError, GiField, MAX_PROBES, ProbeLocalLight, ProbeSun, SSGI_GATHER_HALF, SSGI_GATHER_NATIVE,
    SSGI_GATHER_QUARTER, ScreenSpaceGiSettings, ScreenTexel, SsgiGatherGuard, sample_irradiance,
    screen_space_gather, set_ssgi_settings, ssgi_gather_size, ssgi_gather_step, ssgi_gather_step_used,
    ssgi_quarter_gather, ssgi_settings, ssgi_upsample_low_coord, update_irradiance,
    update_irradiance_with_lights, update_irradiance_with_sky,
};
pub use glass::{
    CANOPY_FULL_ENV, CanopyGlass, CanopyShading, GlassCrackSets, GlassDamage, GlassDamageDraw, GlassImpact,
    GlassInstance, GlassLevelChanged, GlassMask, GlassView, GlassViewMode, apply_glass_damage, canopy_glass,
    set_canopy_glass,
};
pub use gpu_driven::{IndirectStrategy, static_batch_eligible};
pub use graph::{AliasPlan, TransientLifetime};
pub use graph::{
    CompiledGraph, GraphCompileError, GraphContext, GraphExecution, GraphInput, GraphOutput, GraphRecord,
    GraphResource, GraphRun, GraphSlot, RenderGraph, RenderGraphNode, ResourceVersion, TextureDesc,
    TransientEntryInfo, TransientPool, TransientPoolStats, TransientTexture,
};
pub use hair::{HairAlpha, HairFactors};
pub use handle_args::{image_parameter_hazards, specialize_handle_arguments};
pub use headless::HeadlessRender;
pub use host_checklist::{HostCheck, HostChecklist};
pub use ibl::{IblImage, build_brdf_lut, directional_albedo_ggx};
pub use immediate::{
    ImmediateBatch, ImmediateBlend, ImmediateDraw, ImmediateImage, ImmediateImageFormat, ImmediateRenderer,
    ImmediateState, ImmediateStats, ImmediateTarget, ImmediateTexture, ImmediateVertex, UploadKey,
    uploads_vertex_buffers,
};
pub use interior::aerial_interior_pick;
pub use interior::{
    AERIAL_INTERIOR_MIN_SPAN_M, AERIAL_INTERIOR_SLOTS, InteriorRaySpans, aerial_interior_slots,
    interior_span, interior_spans_on_ray,
};
pub use interior::{
    DAYLIT_INTERIOR_BOUNCES, ENCLOSURE_MIN_HITS, EnclosureScene, INTERIOR_AMBIENT_FLOOR_LUX,
    INTERIOR_BOUNCE_GAIN, INTERIOR_EXPOSURE_EV, InteriorCoverage, InteriorLighting, InteriorShade,
    InteriorVolume, build_daylit_interior, build_interior_lighting, detect_enclosure, hollow_box,
    interior_floor_luminance, plan_interior_coverage, shade_for_eye, shade_for_view,
};
pub use lab_frame::{
    ALPHA_ADDITIVE, ALPHA_BLEND, ALPHA_MASK, ALPHA_OPAQUE, ALPHA_PREMULTIPLIED, HardSurfaceFactors,
    LabLocalLight, LabMaterial, LabMesh, LabPointLight, LabScene, LabSpotLight, LabTexture, LabVertex,
    SKY_IBL_SCALE, render_lab_scene, volume_transmittance,
};
pub use lens::PhysicalCamera;
pub use lens::{
    PHYSICAL_CAMERA_FORMAT_VERSION, PhysicalCameraError, physical_camera_migration_sources,
    physical_camera_schema_fingerprint, register_physical_camera_types,
};
/// The `post_volumes` debug overlay: volume shapes and blend bands (ADR-2323).
pub mod post_volume_gizmos;
pub use light_budget::{
    LightDropWarnings, LightSelection, LightUploadReport, LightView, MAX_UPLOADED_LIGHTS,
    ShadowCandidateOrder, assign_clusters, corridor_point_lights, order_shadow_casters, screen_importance,
    select_visible_lights, shadow_view_count,
};
pub use lights::{
    CLUSTER_GRID_X, CLUSTER_GRID_Y, CLUSTER_GRID_Z, DirectionalLight, LightUnits, PointLight, SpotLight,
    cluster_index_3d, cluster_slice_depth, depth_to_cluster_z,
};
pub use local_capture::{
    LOCAL_CAPTURE_FACE, LOCAL_CAPTURE_WIDTH, LocalCapture, LocalCaptureDesc, LocalCaptureError,
    LocalCaptureInfo, MAX_LOCAL_CAPTURES, sync_local_captures,
};
// More captures, the selection and the persisted bake (ADR-2860).
pub use local_capture::{
    LOCAL_CAPTURE_FADE_FRAMES, LOCAL_CAPTURE_HYSTERESIS_M, LOCAL_CAPTURE_LAYERS, LocalCaptureBake,
    LocalCaptureBakes, local_capture_budget, local_capture_mip_count,
};
pub use lut::{ColorLut, LutError};
pub use mask_decals::{
    DecalBlend, DecalSlot, MAX_DECAL_SLOTS, MaskDecalInstance, MaskDecalStats, MaskDecalTextures, MaskDecals,
    MaskPalette, extract_mask_decals,
};
pub use material::{AlphaMode, LayeredStandardMaterial, MaterialLayer, StandardMaterial, UvTransform};
pub use material_clock::{MATERIAL_CLOCK_PERIOD_SECS, material_clock};
pub use memory::{
    CategoryTotal, LabelTotal, MemoryCategory, MemoryReport, memory_report, process_peak_rss_bytes,
    process_rss_bytes,
};
pub use memory_budget::{Admission, BudgetTier, MemoryBudget, MemoryUse, SetUse, admit, memory_use};
pub use mesh_clean::drop_coincident_triangles;
pub use mesh_lod::ClusterLodHandoff;
pub use mesh_lod::{
    DisableMeshLod, LodViewport, MESH_LOD_DISTANCE_SCALE_RANGE, MESH_LOD_FADE_FRAMES,
    MESH_LOD_FADE_OBJECT_TAG, MESH_LOD_HYSTERESIS, MESH_LOD_SHADOW_OBJECT_TAG, MESH_LOD_SHADOW_SCALE_DEFAULT,
    MESH_LOD_SHADOW_SCALE_RANGE, MeshLodBias, MeshLodChain, MeshLodCrossfade, MeshLodLevel, MeshLodMemory,
    MeshLodRestart, MeshLodScales, ShadowLodHint, mesh_lod_cache_key, select_mesh_lods,
};
pub use mesh_occluders::{FAR_BAND_FADE, MAX_FAR_MESH_OCCLUDERS, OccluderBands, far_band_shade};
pub use mesh_occluders::{
    GpuMeshOccluder, MAX_MESH_OCCLUDERS, MeshOccluder, MeshOccluderBlock, OccluderLighting, OcclusionProxy,
    extract_mesh_occluders, polygon_view_factor, reflection_shadow,
};
pub use mesh_storage::{ArenaStats, MeshArena, MeshSlice, ObjectData};
pub use minspec::MinspecProfile;
pub use mist::{
    MIST_GRID_MAX_SIDE, MIST_MAX_SUN_STEPS, MistHeightGrid, MistQuality, ValleyMist, mist_quality,
};
pub use morph::{
    MORPH_GAME_MAX_ACTIVE, MORPH_MESH_KEY_TAG, MeshMorphTargets, MorphInstance, MorphMode, MorphStats,
    MorphTargets, MorphTargetsError, extract_morphs, name_morph_targets,
};
pub use motion_blur::MotionBlur;
pub use offscreen::{clear_to_pixels, clear_to_pixels_format};
pub use oit::OitStats;
pub use outdoor::{
    CloudViewDesc, CloudViewImage, FogShaftImages, OutdoorImage, OutdoorParams, SkyBodyTexture, SkyView,
    cloud_ambient_radiance, render_analytic_sky, render_cloud_coverage, render_cloud_ground_shadow,
    render_cloud_sun_map, render_cloud_view, render_fog_shaft, render_local_fog, render_sky_view,
    render_time_of_day_sky, render_uniform_fog_inscatter,
};
pub use output::{
    HdrOutput, OutputSettings, PresentPolicy, SurfaceChoice, apply_profile_frame_cap, encode_pq_st2084,
    encode_scrgb, horizontal_fov, horizontal_plus_fov_y, select_present_mode, select_surface_format,
};
pub use overlay::{OverlayDraw, SharedOverlay, WindowOverlay};
pub use paged_shadow::{PageId, sequence_flicker, temporal_flicker};
/// Test hook (`test-hooks`): the `PreUpdate` system that queues a boot-tier warm job for the
/// [`WindowOverlay`] layers installed after the [`PipelineWarmQueue`] started (ADR-5131), to run on a
/// test's own world (allocation counts of a steady frame).
#[cfg(feature = "test-hooks")]
pub fn late_overlay_warm_system() -> impl regia_ecs::ecs::system::System<In = (), Out = ()> {
    regia_ecs::ecs::system::IntoSystem::into_system(surface::warm_late_overlay_layers)
}
#[cfg(all(feature = "frame-gen", feature = "test-hooks"))]
pub use surface::{FrameGenTick, feed_frame_gen_tick};

pub use pipeline_warm::{
    GameFrameWarm, GameWarmReport, HostTick, OffscreenWarm, PipelineWarmProgress, PipelineWarmQueue,
    RendererWarm, TierProgress, WARM_FRAME_SIZE, WarmMode, WarmTier, WarmedRenderer, boot_threads,
    default_threads, warm_cancelled,
};
pub use post::{
    ColorGrade, DEFAULT_BLOOM_INTENSITY, DEFAULT_LOOK_CONTRAST, DEFAULT_LOOK_SATURATION, ExposureMeter,
    LocalExposure, PostSettings, TonemapOperator, apply_level_look, bright_coverage_weight,
    correlated_color_temperature, ev100_from_camera, ev100_from_luminance, exposure_from_ev100,
    exposure_from_luminance, pre_exposure_scale,
};
pub use primitives::{
    PRIMITIVE_FORMAT_VERSION, Primitive, PrimitiveCache, PrimitiveError, PrimitiveShape, box_mesh,
    translate_mesh, uv_sphere, uv_sphere_deterministic,
};
pub use probe_specular::{PROBE_SPECULAR_DETAIL_MAX, PROBE_SPECULAR_HUE_FLOOR, ProbeSpecular, SpecularLobe};
pub use profile::{
    AntiAliasing, FrameQuality, GiLook, GiMode, HdrTarget, RenderProfile, RenderProfilePreset, ShadowFilter,
    SpatialUpscaler, SsaoMode, TemporalUpscaler, TerrainBudget, is_steam_deck_model,
};
pub use reflections::{
    EQUIRECT_HEIGHT, EQUIRECT_WIDTH, PROBE_FACE_SIZE, ProbeBlend, ReflectionProbe, box_project,
    capture_quality, cube_face_axes, equirect_from_faces, select_probes, ssr_roughness_weight,
};
pub use render_out::{HudLayout, RenderOutHud, RenderOutSpec};
pub use render_override::{
    ExtractedRenderOverrides, RenderOverride, extract_render_overrides, resolve_render_override,
};
pub use render_thread::{
    RenderThreadError, RenderThreadHost, RenderThreadSettings, RenderThreadStats, with_render_thread,
};
pub use resource_inspector::{
    DumpFormat, DumpedImage, InspectError, ResourceInfo, ResourceKind, Texels, debug_readback, dump_resource,
    list_resources, read_texels, resource_usage, set_debug_readback, usage_names,
};
pub use runtime::{
    CpuMaterial, CpuMesh, Environment, ExtractedCamera, ExtractedFrame, ExtractedMesh, ExtractedTarget,
    ExtractedView, LevelColorLut, MESH_ID_KEY_TAG, MeshLodFade, MeshRefresh, MeshRenderer, MeshSource,
    MeshUv1, NotShadowCaster, RUNTIME_INSTANCE_RESERVE, RenderSettings, RuntimeRenderer, SkinJoints,
    TEXTURE_TARGET_SIZE, ViewTables, alias_mesh_key, extract_frame, extract_glass, load_level_lut,
    mesh_content_revision, mesh_key, mesh_refresh, note_mesh_content_changed, register_render_types,
    render_extracted_into, render_texture_views, spawn_lab_scene, sync_view_tables, take_released_mesh_keys,
    with_unique_mesh_mut,
};
pub use scalar_overlay::{
    BrushFalloff, BrushMode, BrushSeed, ExtractedScalarOverlays, OverlayFit, RAMP_LUT_SIZE, RAMP_NAMES, Ramp,
    RampError, RampStop, ScalarOverlay, ScalarOverlayStats, VertexAdjacency, apply_brush,
    extract_scalar_overlays, ramp_position,
};
pub use scene_export::{ExportedInstance, FrameExport, ReferenceHeightfield};
pub use screenshot::{CapturedImage, ScreenshotCallback, ScreenshotOptions, ScreenshotRequests};
pub use sdf::{HeightfieldGrid, MeshSdf, SdfScene, SdfShape, heightfield_distance, sphere_trace};
pub use shader_check::{
    SPIRV_VAL_FALLBACK_ENV, composed_function_layout_errors, spirv_explicit_layout_violations,
    validate_composed_modules, validate_engine_shaders, validate_graph_code_spirv,
    validate_ray_query_spirv_module, validate_spirv_module, validate_surface_graph, validate_wgsl_modules,
    wgsl_function_layout_errors,
};
pub use shader_lib::{cloud_shadow_wgsl, compose_source, compose_source_with, wgsl_source};
pub use shading_model::{
    ExtractedMaterialShading, EyeShading, FabricSheen, MaterialShading, ShadingModel, SkinShading,
    extract_material_shading,
};
pub use shadow::{
    CSM_FIRST_CASCADE_MAX, CSM_LAST_SPLIT_MAX, CSM_SINGLE_SPLIT_MAX, NEAR_SUN_RANGE_M,
    NEAR_SUN_TILE_MARGIN_TEXELS, cascade_splits, csm_boundaries, frustum_slice_sphere,
    layout_shadow_tiles_reserved, snap_ortho_center, snap_to_texels,
};
pub use shared_draw::{
    GROUND_BOUNCE_LEVELS, GROUND_BOUNCE_MAX, GroundBounce, GroundBounceLevel, SharedFrameInfo, SharedPass,
    SharedPassDraw, SharedUpload,
};
pub use shared_lights::{
    EngineLights, SHARED_LIGHTS_BINDINGS, SHARED_LIGHTS_FRAME_BYTES, SHARED_LIGHTS_LIGHT_BYTES,
    SHARED_LIGHTS_STORAGE_BUFFERS, SHARED_LIGHTS_TILE_BYTES, SHARED_LIGHTS_UNIFORM_BUFFERS,
    shared_lights_layout_entries, shared_local_lights_wgsl,
};
pub use shared_lights::{shared_light_profiles_layout_entry, shared_local_lights_profiled_wgsl};
pub use skin::{
    DualQuatJoint, JointMatrix, MAX_INFLUENCES_COMPUTE, MAX_INFLUENCES_PER_VERTEX, MAX_JOINTS_PER_SKIN,
    SkinPalette, SkinnedFrame, SkinningMethod, skin_position, skin_vertex_reference,
};
pub use skin_compute::{SKIN_TWIN_KEY_TAG, SkinningStats, skin_on_gpu};
pub use skinning::{
    ExtractedSkinning, SkinInfluences8, SkinningConfig, SkinningOptions, extract_skinning,
    resolve_skinning_options,
};
pub use sssr::{
    SSSR_COST_BOUND_ENV, SssrCostBound, SssrCostCounters, set_sssr_cost_bound, set_sssr_cost_probe,
    sssr_cost_bound, sssr_cost_bound_env, sssr_cost_probe,
};
pub use sssr::{
    SSSR_HALF_RES_MAX_STEPS, SSSR_HALF_RES_ROUGHNESS, SSSR_MAX_STEPS, SSSR_ROUGHNESS_BAND,
    SSSR_ROUGHNESS_CUTOFF, SSSR_ROUGHNESS_CUTOFF_ENV, SSSR_ROUGHNESS_CUTOFF_RANGE, SSSR_THICKNESS, SssrStats,
    pack_surface_code, set_sssr_roughness_cutoff, sssr_roughness_cutoff, sssr_roughness_cutoff_env,
    unpack_surface_code,
};
pub use studio_env::{
    EnvironmentAuthority, STUDIO_ENV_DEFAULT_BUDGET, STUDIO_ENV_STANDARD_WIDTH, STUDIO_ENV_STUDIO_WIDTH,
    STUDIO_EQUIRECT_MAX_TEXELS, STUDIO_FALLOFF_RANGE, StudioBackground, StudioEnvError, StudioEnvInfo,
    StudioEnvResolution, StudioEquirect, StudioGradient,
};
pub use submission::SubmissionStats;
pub use subsurface::{SubsurfaceDrawFacts, SubsurfaceKernel, SubsurfaceUnsupported};
pub use subsurface::{SubsurfaceFactors, SubsurfaceProfile, SubsurfaceStats};
pub use surface::{
    DeviceLostHook, DeviceLostHooks, WindowSurface, WindowSurfaces, encode_clear, pixel_viewport,
};
pub use surface_graph::{GraphCode, GraphTexture, SurfaceGraph};
pub use surface_weather::{
    PrecipOcclusionBinding, PrecipOcclusionInfo, PrecipOcclusionMap, PrecipOcclusionRequest,
    PrecipOcclusionView, ResolvedWeatherResponse, SunOcclusionRequest, SurfaceSample, SurfaceWeatherFrame,
    WeatherResponse, apply_surface_weather, derived_porosity, puddle_key, ripple_gradient,
    surface_weather_wgsl, value_noise,
};
pub use tangents::{generate_vertex_normals, generate_vertex_tangents, tangent_passes_on_this_thread};
pub use texture_budget::{
    PROBLEM_TEXTURE_BUDGET, PROBLEM_UNCOMPRESSED_TEXTURES, PROBLEM_WRONG_PLATFORM_TEXTURES,
    TextureMemoryEntry, TextureMemoryReport, TextureMemorySummary, UNCOMPRESSED_TEXTURE_LIMIT,
    WrongPlatformTextures, name_material_textures, report_wrong_platform_texture, resident_cap,
    set_resident_cap, wrong_platform_textures,
};
pub use texture_residency::{
    DynamicTexture, DynamicTextureError, MaterialImage, MaterialImages, SharedImage, SharedImageError,
};
pub use texture_stream::{StreamReport, TextureStreaming};
pub use thin_film::{ThinFilmFactors, ThinFilmStats, pack_thin_film_map};
pub use tonemap::{display_look, inverse_tonemap_agx, tonemap_agx};
pub use translucent::{LayerCamera, TranslucentLayer};
pub use tsr::TsrSettings;
pub use upload_queue::{UploadBytes, UploadQueue, UploadStats};
pub use upscaler::{UpscaleQuality, UpscalerFeatures, UpscalerKind, UpscalerStatus, VendorUpscaler};
pub use visbuffer::{
    DenseGateReport, DenseSceneSelect, MeshletBuilder, MeshletMesh, SceneDensity, SinglePassFallback,
    VisBufferCounters, VisBufferSettings, VisBufferStats, VisInstanceSet, VisResolveReport,
    VisibilityDefault, VisibilityDefaultState, VisibilityPath, VisibilitySelect,
};
pub use volumetrics::{
    FOG_LAYER_RANGE_M, FogCone, FogLight, FogVolume, FogVolumes, SunScattering, VolumetricFogSettings,
    blend_history, extinction_at, fog_layer_optical_depth, fog_light_falloff, froxel_depth, froxel_slice_uvw,
    henyey_greenstein, inscatter_per_metre, integrate_segment, reproject_froxel,
};
// The frame's pass extension point (ADR-1552).
pub use frame::FrameGraphBuffer;
pub use frame::{
    FRAME_STEPS, FrameGraphReport, FramePass, FramePassContext, FramePassInfo, FrameStep, PassRegisterError,
    SlotColorSpace, slot_color_space, slot_from_name, slot_name,
};
pub use frame::{
    FrameDispatchInfo, FrameDispatchViolation, FrameGraphError, FrameGraphInfo, FrameGraphNode,
    SunTraceReport,
};
// Runtime lens effects: ghost flare, dirt, chromatic aberration (ADR-1863).
pub mod lens_effects;
pub use lens_effects::{
    LENS_EFFECTS_FORMAT_VERSION, LensEffectsError, lens_effects_migration_sources,
    lens_effects_schema_fingerprint, register_lens_effects_types,
};
pub use lens_effects::{LensEffects, LensEffectsPass, gate_lens_effects};
// Two-phase occlusion of the shadow casters (specs/98 § 10a, ADR-2072).
mod shadow_occlusion;
pub use shadow_occlusion::{
    SHADOW_OCCLUSION_KINDS, SHADOW_OCCLUSION_MIN_CASTERS, ShadowKindOcclusion, ShadowOcclusionCounters,
    ShadowOcclusionSkip, ShadowOcclusionStats, ShadowPhaseCounts, shadow_kind_name,
};
pub use shadow_occlusion::{ShadowOcclusionWork, shadow_occlusion_work};
// Rough transmission and spatiotemporal blue noise (ADR-2075).
pub mod blue_noise;
pub mod rough_transmission;
// Video frames and the GPU video convert pass (specs/50 § 2.7, ADR-2044); off unless a host installs them.
pub mod video;
pub mod video_convert;
pub use video::{
    VideoControl, VideoControlError, VideoFrameInfo, VideoFrameSource, VideoFrames, VideoPlayback, VideoSlot,
    VideoSystems, YuvMatrix, YuvPlanes, video_material,
};
pub use video_convert::{
    OwnedYuv, VIDEO_CONVERT_PASS_NAME, VideoConvertJobs, VideoConvertPass, VideoConvertStats, VideoConverter,
    VideoTarget,
};
pub mod video_fullscreen;
pub use video_fullscreen::{
    FullscreenFit, FullscreenVideo, VIDEO_FULLSCREEN_PASS_NAME, VideoFullscreenPass, VideoFullscreenQueue,
};
// The forward shader's frame-level studio axis (ADR-2308).
pub use forward_permutations::{
    FORWARD_STUDIO_ENV, ForwardStudioMode, ForwardStudioReport, StudioFeatures,
    forward_studio_env_forces_full,
};
// Post-volume shapes beyond box and sphere, gizmo outlines, per-volume LUT blending (ADR-2323).
pub mod post_volume_luts;
pub mod post_volume_shapes;
pub use post_volume_luts::PostVolumeLuts;
pub use post_volume_shapes::{VolumeOutline, post_volume_outline};

/// Colour every window is cleared to, linear RGBA.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct ClearColor(pub [f64; 4]);

impl Default for ClearColor {
    fn default() -> Self {
        Self([0.02, 0.02, 0.025, 1.0])
    }
}

/// The window's frame in the `Render` schedule. A host that draws views of its own orders them
/// before it.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowRenderSet;

/// Renderer errors.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// No GPU adapter, not even a software one.
    #[error("no compatible GPU adapter found: {0}")]
    NoAdapter(wgpu::RequestAdapterError),
    /// The adapter refused to create a device.
    #[error("cannot create GPU device: {0}")]
    RequestDevice(wgpu::RequestDeviceError),
    /// The platform layer has not provided a display handle yet.
    #[error("no display handle (the window system is not up yet)")]
    NoDisplay,
    /// The OS window could not be turned into a surface.
    #[error("cannot create surface: {0}")]
    CreateSurface(wgpu::CreateSurfaceError),
    /// The surface exists but the adapter offers no format to present it with.
    #[error("cannot present to this window: {0}")]
    SurfaceUnsupported(String),
    /// Waiting for the GPU failed.
    #[error("GPU poll failed: {0}")]
    Poll(wgpu::PollError),
    /// Reading a texture back failed.
    #[error("texture readback failed: {0}")]
    Readback(String),
    /// A WGSL module failed to compose or validate.
    #[error("shader composition failed: {0}")]
    Shader(String),
    /// `REGIA_GPU_STRICT` is set and the requested GPU could not be selected.
    #[error("GPU selection failed: {0}")]
    GpuSelection(String),
    /// A frame's command encoding failed (an encode worker exited without its buffer).
    #[error("command encoding failed: {0}")]
    Encode(String),
    /// A texture upload does not fit in the staging belt.
    #[error("texture upload too large: {0}")]
    UploadTooLarge(String),
}

/// Device, surfaces and the clear pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderPlugin {
    /// When true, window surfaces are not presented (`specs/13` § 12).
    pub headless: bool,
}

impl Plugin for RenderPlugin {
    fn build(&self, app: &mut App) {
        let _mem = regia_core::memory::enter_owner(&crate::heap_owners::PLUGIN);
        app.init_resource::<ClearColor>()
            .init_resource::<OutputSettings>()
            .init_resource::<WindowSurfaces>()
            .init_resource::<DeviceLostHooks>()
            .init_resource::<WindowOverlay>()
            .init_resource::<ScreenshotRequests>()
            .init_resource::<HeadlessRender>()
            .init_resource::<runtime::ExtractedFrame>()
            .init_resource::<draw_list::DrawList>()
            .init_resource::<runtime::RuntimeRenderer>()
            .init_resource::<runtime::LevelColorLut>()
            .init_resource::<post_volume_luts::PostVolumeLuts>()
            .init_resource::<color_vision::ColorVision>()
            .init_resource::<PipelineWarmQueue>()
            .init_resource::<GameFrameWarm>()
            .init_resource::<scene_ready::SceneReadiness>()
            .add_message::<scene_ready::SceneReady>()
            .init_resource::<QualityProbe>()
            .init_resource::<BootBackground>()
            .init_resource::<UploadQueue>()
            .init_resource::<shader_reload::ShaderReloadJobs>()
            .add_message::<regia_asset::SourceAssetChanged>()
            .add_systems(Update, shader_reload::reload_engine_shaders)
            .add_systems(Startup, permutation_policy::install_startup_guard)
            .add_systems(Startup, brdf_energy::apply_profile_model)
            .add_systems(Startup, forward_decals::apply_profile_switch)
            .init_resource::<primitives::PrimitiveCache>()
            .register_type::<primitives::Primitive>()
            .register_type::<primitives::PrimitiveShape>()
            .insert_resource(HeadlessRender(self.headless))
            .add_plugins(gizmos::GizmoPlugin);
        stream_hooks::add_to_app(app);
        // Dev builds also watch the engine's WGSL (only exists in a source checkout).
        if let Some(shaders) = shader_reload::engine_shader_dir() {
            app.world_mut()
                .get_resource_or_init::<regia_asset::DevWatchRoots>()
                .0
                .push(shaders);
        }
        runtime::register_render_types(app);
        exit::register(app);
        frame_gen::register(app, self.headless);
        // The paused-scene hold's host switch (ADR-7890).
        app.init_resource::<scene_hold::SceneHold>();
        // A hold that follows the gameplay pause (ADR-10105) sees `Time::paused` before the frame.
        app.add_systems(PostUpdate, scene_hold::follow_pause);
        boot::mark(BootMark::PluginBuild);
        // Out-of-the-box display calibration for this platform (ADR-092); the framework's
        // settings replace it with the player's values.
        let platform = app
            .world()
            .get_resource::<DisplayPlatform>()
            .copied()
            .unwrap_or_else(DisplayPlatform::current);
        if !app.world().contains_resource::<DisplayPlatform>() {
            app.insert_resource(platform);
        }
        if !app.world().contains_resource::<DisplayCalibration>() {
            app.insert_resource(DisplayCalibration::platform_default(platform));
        }
        app.add_systems(PreUpdate, calibration::query_display_luminance);
        // Drops the pooled shader composers once composition goes quiet (ADR-3351).
        app.add_systems(Last, shader_lib::release_idle_composers);
        // Greybox primitives become `MeshRenderer`s before the frame is extracted.
        app.add_systems(PostUpdate, primitives::sync_primitive_meshes);
        // Extraction always runs (headless tests assert on the extracted frame;
        // agent offscreen captures `seeing.screenshot` target "offscreen" rely on it);
        // texture-target cameras render wherever a device exists.
        app.init_resource::<glass::GlassCrackSets>()
            .add_message::<glass::GlassLevelChanged>()
            .register_type::<glass::GlassView>()
            .register_type::<glass::GlassViewMode>()
            .register_type::<glass::GlassDamage>()
            .register_type::<glass::GlassImpact>()
            .register_type::<mesh_occluders::OcclusionProxy>();
        app.init_resource::<mesh_lod::LodViewport>();
        app.init_resource::<mesh_lod::ClusterLodHandoff>();
        app.init_resource::<render_override::ExtractedRenderOverrides>();
        app.init_resource::<shading_model::ExtractedMaterialShading>();
        app.add_systems(PreUpdate, morph::name_morph_targets);
        app.init_resource::<skinning::ExtractedSkinning>();
        // Debug view modes (wireframe) append to the extracted gizmos once they are final.
        app.init_resource::<debug_view::DebugView>().add_systems(
            Render,
            debug_view::apply_debug_view
                .after(gizmos::GizmoSystems::Finish)
                .before(runtime::render_texture_views),
        );
        // Local reflection captures on entities (ADR-2000), before the frame that bakes them.
        app.add_systems(
            Render,
            local_capture::sync_local_captures.before(runtime::extract_frame),
        );
        app.init_resource::<scalar_overlay::ExtractedScalarOverlays>();
        app.init_resource::<fog_lights::FogLightPick>();
        // What the froxel fog read for the sun shadow (`render.fog_shadow`, ADR-3122).
        app.init_resource::<fog::FogSunShadowProbe>();
        // Lightmap receivers (ADR-1623): the swapped meshes and UV2 streams before the extraction.
        app.init_resource::<lightmap::LightmapReceiverReport>();
        app.add_systems(
            Render,
            lightmap::apply_lightmap_receivers.before(runtime::extract_frame),
        );
        // Color-vision filter (ADR-2646): composed into each view's LUT right after extraction.
        app.add_systems(
            Render,
            color_vision::apply_color_vision
                .after(runtime::extract_frame)
                .before(mesh_lod::select_mesh_lods),
        );
        app.add_systems(
            Render,
            (
                runtime::extract_frame,
                mesh_lod::select_mesh_lods,
                glass::apply_glass_damage,
                runtime::extract_glass,
                mesh_occluders::extract_mesh_occluders,
                render_override::extract_render_overrides,
                scalar_overlay::extract_scalar_overlays,
                mask_decals::extract_mask_decals,
                shading_model::extract_material_shading,
                morph::extract_morphs,
                skinning::extract_skinning,
            )
                .chain(),
        )
        // Culling writes `ViewVisibility` once the frame's draws are final (ADR-7200).
        .init_resource::<visibility::ViewVisibilityStats>()
        .init_resource::<visibility::ViewVisibilityPerView>()
        .add_systems(
            Render,
            visibility::update_view_visibility
                .after(skinning::extract_skinning)
                .before(runtime::render_texture_views),
        )
        // The two-phase cull's result, read back without a stall into `ViewVisibility` (ADR-7255).
        .init_resource::<occlusion_feedback::OcclusionFeedback>()
        .add_systems(
            Render,
            occlusion_feedback::connect_occlusion_feedback.before(runtime::render_texture_views),
        )
        .add_systems(
            PreUpdate,
            (
                runtime::sync_profile_frame_cap,
                surface::sync_surface_output,
                runtime::load_level_lut,
                post_volume_luts::load_post_volume_luts,
                surface::track_boot_tiers,
                boot::track_boot_background,
                device::publish_software_renderer,
            ),
        );
        // Indoor cells and portals: the per-view walk drops unreachable cells' draws after the
        // mesh LOD and before `ViewVisibility` (spec 25 § 7.7, ADR-7586).
        viscells::install(app);
        if !self.headless {
            app.add_systems(Lifecycle, surface::on_lifecycle)
                .add_systems(
                    PreUpdate,
                    (surface::track_resizes, surface::warm_late_overlay_layers),
                )
                .add_systems(
                    Render,
                    (
                        surface::handle_device_lost.run_if(gpu_health::device_lost),
                        (
                            upload_queue::pump_uploads,
                            // Before the texture views: a new prepare holds them too (ADR-8122).
                            scene_ready::take_scene_prepare,
                            runtime::render_texture_views,
                            surface::render_windows.in_set(WindowRenderSet),
                            scene_ready::publish_scene_ready,
                        )
                            .chain()
                            .run_if(gpu_health::device_ok),
                    )
                        .chain()
                        .after(mesh_occluders::extract_mesh_occluders)
                        .after(render_override::extract_render_overrides)
                        .after(scalar_overlay::extract_scalar_overlays)
                        .after(mask_decals::extract_mask_decals)
                        .after(shading_model::extract_material_shading)
                        .after(morph::extract_morphs)
                        .after(skinning::extract_skinning),
                );
        } else {
            app.add_systems(
                Render,
                (
                    upload_queue::pump_uploads,
                    // Before the texture views: a new prepare holds them too (ADR-8122).
                    scene_ready::take_scene_prepare,
                    runtime::render_texture_views,
                    scene_ready::advance_headless_scene_ready,
                    scene_ready::publish_scene_ready,
                )
                    .chain()
                    .after(mesh_occluders::extract_mesh_occluders)
                    .after(render_override::extract_render_overrides)
                    .after(scalar_overlay::extract_scalar_overlays)
                    .after(mask_decals::extract_mask_decals)
                    .after(shading_model::extract_material_shading)
                    .after(morph::extract_morphs)
                    .after(skinning::extract_skinning),
            );
        }
    }
}
// Foliage sway on ordinary materials: imported trees and scattered plants (`specs/88` § 2.5, ADR-2304).
mod foliage_sway_material;
pub use foliage_sway_material::{FOLIAGE_SURFACE, FOLIAGE_SURFACE_FLAGS_SLOT, SwayAdoptError};
// Stochastic many-light sampling for Desktop Ultra: ReSTIR-style direct lighting (ADR-2470).
pub mod restir_di;
// The direct BRDF's energy model: opt-in Kulla-Conty compensation (ADR-2632).
pub mod brdf_energy;
pub use brdf_energy::{BRDF_ENERGY_ENV, BrdfEnergy, process_brdf_energy, set_process_brdf_energy};
// IES light profiles and authored rect and tube lights (spec 13 § 6, ADR-2592).
pub mod light_profiles;
// Color-vision filters, simulation and compensation, through the view LUT (ADR-2646).
pub mod color_vision;
// Stochastic many-light sampling in the production frame (ADR-2652).
pub use frame::{ManyLightReport, ManyLightSkip};
// The spec 27 § 2.3 temporal chart (`seeing.temporal_chart`, ADR-2638).
pub mod temporal_chart;
// Device-independent frame work counts and the per-preset ratchet (ADR-2711).
pub mod frame_work;
pub use frame_work::{FrameWork, FrameWorkProbe, WorkCounters};
// The ground's haze and the weather from the atmosphere tables, and the horizon seam (ADR-3294).
pub mod horizon_seam;
pub use outdoor::{HorizonPlate, HorizonPlateImage, HorizonWeather, TableAir, render_horizon_plate};
// ReSTIR GI over hardware ray queries (spec 27 stage 2b, ADR-3349).
pub mod restir_gi;

pub mod budget_ledger;

// The reference overlay of the look reviews (spec 27 § 5, ADR-2463).
pub mod reference_overlay;
pub use reference_overlay::{
    REFERENCE_OVERLAY_PASS_NAME, ReferenceImage, ReferenceOverlayHandle, ReferenceOverlayMode,
    ReferenceOverlayPass, ReferenceOverlaySettings, ReferenceOverlayStats,
};
// Normal and roughness AOVs of the real frame through the forward debug outputs (ADR-2951).
pub mod frame_aov;
pub use frame_aov::{FrameAov, capture_frame_aov};
// The shader permutation matrix and the Metal translation check (`check-shaders --matrix`, ADR-2644).
pub mod shader_msl_check;
pub use shader_check::{
    BackendResult, BackendStatus, ForwardPermutation, MATRIX_TEXTURE_TABLE_SIZE, MatrixBackend, MatrixInput,
    MatrixModule, MatrixOrigin, MatrixPermutation, MatrixScope, PresetFrameResult, SHADER_MATRIX_JOBS_ENV,
    SHADER_MATRIX_REPORT_VERSION, ShaderMatrixOptions, ShaderMatrixReport, default_matrix_jobs, define_key,
    forward_permutations_for, run_matrix_inputs, run_shader_matrix, shader_matrix_inputs,
    validate_preset_frames,
};
// Shipped forward-shader cost per profile and its bytes budget (ADR-2664).
pub use shader_check::{
    SHADER_COST_REPORT_VERSION, ShaderBytesBudget, ShaderCostReport, ShaderCostRow, shader_cost_report,
    shader_cost_row,
};
// Rust and WGSL struct agreement: layouts from naga, a registry of rows, the ratchet (ADR-3002).
pub mod shader_contract;
pub use shader_contract::{
    AllowEntry, BindingSpace, BoundStruct, ContractInput, ContractReport, ContractRow, ContractSummary,
    StructLayout, bound_structs, contract_file_name, ratchet_sources, render_contract_modules,
    run_contract_ratchet, struct_layouts,
};
// Binding counts per stage against the limits of each device class (ADR-3002).
pub mod shader_limits;
pub use shader_limits::{
    BindingCounts, LimitClass, LimitNumber, LimitsReport, MetricUse, PresetLimits,
    SHADER_LIMITS_REPORT_VERSION, StageUsage, limit_classes, module_usage, run_shader_limits,
};
// Material-graph pipeline sets against the device-class limits at cook (ADR-4901).
pub use shader_limits::{GraphModuleUsage, graph_code_usage, graph_limit_check, preset_limit_class};
mod shader_contract_registry;
pub use shader_contract_registry::{render_contract_allowlist, render_contract_rows};
// The reactive mask's sources and the translucent layers' place against the resolve (ADR-3101).
pub mod reactive_mask;
// Forward decals: projected decals blended into the surface before lighting (ADR-3255).
pub mod forward_decals;
pub use forward_decals::{
    ForwardDecal, ForwardDecalAtlas, ForwardDecalFeed, ForwardDecalStats, ForwardDecalView,
    process_forward_decals, set_process_forward_decals,
};
// The renderer's named CPU heap owners for memory.report and memory.idle_ledger (ADR-3351).
pub mod heap_owners;
// The shader composer cache's counters and release (ADR-3351).
pub use shader_lib::{
    COMPOSER_IDLE_FRAMES, ComposerCacheStats, composer_cache_stats, release_composer_cache,
    release_idle_composers,
};
// The cluster index buffer's size by the frame's lights-per-cluster cap (ADR-3351).
pub use lights::{CLUSTER_COUNT, cluster_index_bytes, cluster_index_capacity};
// The material sampler's anisotropic mip blend, calibrated per device (ADR-5670).
pub mod material_aniso_calibration;
pub use material_aniso_calibration::{
    MATERIAL_ANISO_LOD_OFFSET_ENV, last_material_anisotropy_lod_offset, material_anisotropy_lod_offset,
};
// More than one directional light, ambient volumes and the shader debug views (ADR-7211).
pub mod lights_views;
pub use lights_views::{
    AmbientVolume, FrameAmbientVolume, LightsViews, LightsViewsUniform, MAX_AMBIENT_VOLUMES,
    MAX_DIRECTIONAL_LIGHTS, MAX_EXTRA_DIRECTIONAL,
};
// The overdraw and LOD-coloring debug views (ADR-7211).
pub mod debug_shading;
// The Android storage-buffer floor checked at device creation (`specs/13` § 2, ADR-7211).
pub mod storage_floor;
// Hidden things stop drawing; culling writes `ViewVisibility` (spec 14 § A1, ADR-7200).
pub mod visibility;
pub use visibility::{
    DrawVisibility, ViewVisibilityPerView, ViewVisibilityStats, ViewVisibilityViewStats, is_drawn,
    update_view_visibility,
};
// Indoor occlusion by cells and portals, `<map>.viscells.ron` v1 (spec 25 § 7.7, ADR-7586).
pub mod viscells;
pub use viscells::{VISCELLS_FORMAT_VERSION, VisCellSettings, VisCellStats, VisCells, VisCellsError};

// GPU upload of counted built-in meshes: the `PrepareAsset` for `regia_asset::CpuMesh` (ADR-7256).
pub mod asset_upload;
pub use asset_upload::{
    ASSET_MESH_KEY_TAG, ASSET_MESH_VERTEX_STRIDE, AssetMeshBuffers, AssetMeshUploads, asset_mesh_key,
    is_asset_mesh_key,
};
// Counted (GPU-only) meshes and textures in an App: the uploaders' plugin and counted texture
// uploads (ADR-7330).
pub mod asset_texture_upload;
pub use asset_texture_upload::{
    AssetCookedTexturePrepare, AssetImage, AssetImagePrepare, AssetTextureUploads,
};
pub mod counted_assets;
pub use counted_assets::{CountedAssetUploads, CountedMeshPlugin};
pub use runtime::{counted_mesh, counted_mesh_id};
// The two-phase occlusion result, read back without a stall for `ViewVisibility` (ADR-7255).
pub mod occlusion_feedback;
pub use occlusion_feedback::{OcclusionFeedback, OcclusionFeedbackStats, OcclusionViewStats};
// The debug labels' glyph-source hook: a font atlas regia_ui installs (ADR-7254).
pub mod debug_labels;
pub use debug_labels::{
    DebugLabelGlyph, DebugLabelGlyphSource, DebugLabelGlyphs, DebugLabelLayout, DebugLabelPage,
    install_debug_label_glyphs,
};

/// One-shot read-only frame debugger checkpoints through public pass extensions.
pub mod frame_capture;
// Scene readiness: the host's scene prepare and the one "scene presented" edge (ADR-7883).
pub mod scene_ready;
pub use scene_ready::{
    SCENE_PREPARE_MAX_ROUNDS, SceneReadiness, SceneReady, SceneReadyState, SceneReadyStatus,
};
// The paused-scene hold: keep the last scene image and redraw only the UI (ADR-7890).
pub mod scene_hold;
pub use scene_hold::{RetainedScene, SceneHold, SceneHoldFrame};
// The per-frame load budget: late uploads and pipeline builds past a cap wait (ADR-7884).
pub mod load_budget;
pub use load_budget::{FrameLoadBudget, LateDraw, LoadBudgetReport};
// Keyed draws without one entity per draw, appended by `extract_frame` (ADR-8087).
pub mod draw_list;
pub use draw_list::{DrawKey, DrawList, DrawListStats, DrawLodStats, DrawSyncReport, KeyedDraw};
// Vertex-cluster far LOD for multi-piece meshes: one shared grid, closed seams (ADR-8464).
pub mod vertex_cluster;
pub use vertex_cluster::{
    ClusterLod, ClusterPiece, ClusteredPiece, VertexClusterError, VertexClusterSettings, cluster_cpu_meshes,
    cluster_lab_mesh, cluster_pieces, clustered_cpu_mesh, clustered_lab_vertices,
};

// Fog of war world overlay: the viewer's fog texture over the scene (audit gap FW-1).
pub mod fog_overlay;
pub use fog_overlay::{
    FOG_OVERLAY_PASS_NAME, FOG_OVERLAY_WGSL, FogLook, FogOverlayHandle, FogOverlayPass, FogOverlayStats,
};
