// SPDX-License-Identifier: MPL-2.0
//! Hardware ray tracing (ADR-515, ADR-1149, ADR-2035).
//!
//! wgpu 30 exposes ray queries as `Features::EXPERIMENTAL_RAY_QUERY` (Vulkan only, native only,
//! behind an `unsafe` experimental-features token). The first ray-traced feature is the sun's
//! shadow, off in every preset and enabled per profile
//! ([`crate::RenderProfile::sun_shadow_trace`]):
//!
//! - [`probe`]: what the adapter offers, whether the profile asked for it, and the device request
//!   that follows. Off unless asked: the default device never carries an experimental feature.
//! - [`accel`]: the acceleration-structure build abstraction. A CPU planner (instances to
//!   bottom-level structures, dedupe, rebuild tracking, limit checks) that every test can run,
//!   and a GPU builder that exists only on a device with ray queries enabled.
//! - [`bvh`] and [`shadow`]: the compute fallback. A stackless BVH (skip pointers) traced by a
//!   plain compute shader, so shadow rays run on every adapter, lavapipe included, and serve as
//!   the reference the ray-query path is compared against.
//!
//! - [`variant`]: the sun-shadow shader variants (shadow map, ray queries, compute fallback,
//!   screen-space trace), the choice between them, and the forward-shader defs and pipeline-key
//!   bits each one compiles with.
//! - [`sun_trace`]: the frame's visibility pass: world positions from the depth buffer, one ray
//!   per pixel through the variant's tracer, the output the forward pass reads.
//! - [`adapters`]: every adapter's offer without creating a device (`regia doctor`).
//! - [`casters`]: casters outside the frame's mesh list (a shared draw's proxy, persistent
//!   instance sets), the refit-or-rebuild choice, the masked-caster proxy and the rule for
//!   skipping unread shadow maps (ADR-2310).
//!
//! Nothing here runs in a frame unless the profile asks for it.

pub mod accel;
pub mod adapters;
pub mod bvh;
pub mod casters;
pub mod probe;
pub mod shadow;
pub mod sun_trace;
pub mod variant;

pub use accel::{
    AccelBuildStats, AccelError, AccelInstance, AccelMesh, AccelPlan, AccelPlanError, AccelPlanner, BlasPlan,
    GpuAccel, GpuAccelMesh, TlasInstancePlan, tlas_transform,
};
pub use adapters::{AdapterRayTracing, probe_adapters};
pub use bvh::{BvhError, BvhNode, BvhTriangle, ShadowBvh, any_hit, triangle_coverage_byte};
pub use casters::{
    MASKED_COVERAGE, SceneChange, SceneSignature, SharedTraceCasters, SunMapReaders, TraceCasterInstance,
    TraceCasterMesh, classify_scene_change, sun_map_keep_mask,
};
#[cfg(feature = "test-hooks")]
pub use probe::record_device_report_for_test;
pub use probe::{
    AccelLimits, HwrtDecision, HwrtReason, HwrtReport, HwrtRequest, RayTracingProbe, ShadowRayPath,
    device_request, last_device_report, set_device_request,
};
pub use shadow::{
    FALLBACK_SHADOW_WGSL, RAY_QUERY_SHADOW_WGSL, ShadowFallbackPass, ShadowFallbackScene, ShadowParams,
    ShadowRayQueryPass, ShadowTargets, create_shadow_visibility, trace_shadow_rays_cpu,
    upload_shadow_origins,
};
pub use sun_trace::{SUN_TRACE_BVH_WGSL, SUN_TRACE_RAY_QUERY_WGSL, SUN_TRACE_SCREEN_WGSL};
pub use variant::{
    FrameTraceSupport, RtShadowRequest, SUN_SHADOW_RT_DEF, SunShadowReason, SunShadowSelection,
    SunShadowVariant, TraceShader, frame_sun_shadow_selection, select_sun_shadow_variant,
};
