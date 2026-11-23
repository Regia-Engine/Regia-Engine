// SPDX-License-Identifier: MPL-2.0
//! Sun-shadow shader variants: which tracer feeds the sun's visibility, and the shader defs and
//! pipeline-key bits that follow (ADR-1149, ADR-2035).
//!
//! Four variants exist:
//!
//! - [`SunShadowVariant::ShadowMap`]: the cascaded shadow map, what every shipped preset draws.
//! - [`SunShadowVariant::RayQuery`]: a trace with hardware ray queries writes each pixel's sun
//!   visibility ([`super::sun_trace`]).
//! - [`SunShadowVariant::ComputeFallback`]: the same trace through the compute BVH. It runs on
//!   every adapter, so it is the reference and the path tests and lavapipe use. It is not tuned for
//!   a frame budget.
//! - [`SunShadowVariant::ScreenTrace`]: a march through the depth buffer, where hardware ray
//!   queries are unavailable. It keeps the shadow map and adds what the march finds.
//!
//! Both ray-traced variants compile the forward shader with [`SUN_SHADOW_RT_DEF`]. Under that
//! def, opaque surfaces take their sun visibility from the screen visibility texture (the
//! forward pass's binding 22, which contact shadows write when the variant is the shadow map)
//! instead of from the shadow map. Blended surfaces are not in the depth the trace starts
//! from, so they keep the shadow map.
//!
//! The choice is a pure function of the request ([`crate::RenderProfile::sun_shadow_trace`]) and
//! the device ([`select_sun_shadow_variant`]) and of what the frame can draw
//! ([`frame_sun_shadow_selection`]), so every branch is tested without ray-tracing hardware.

use std::collections::HashMap;

use naga_oil::compose::ShaderDefValue;

use super::probe::{HwrtReport, HwrtRequest, ShadowRayPath};
use super::sun_trace::{SUN_TRACE_BVH_WGSL, SUN_TRACE_RAY_QUERY_WGSL, SUN_TRACE_SCREEN_WGSL};
use crate::RenderProfilePreset;

/// Forward-shader def of the ray-traced variants (`lab_forward.wgsl`).
pub const SUN_SHADOW_RT_DEF: &str = "SUN_SHADOW_RT";

/// What a render profile asks of the sun's shadows ([`crate::RenderProfile::sun_shadow_trace`]).
/// Off in every preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RtShadowRequest {
    /// The shadow map (the default).
    #[default]
    Off,
    /// Hardware ray queries when the device has them; otherwise the screen-space trace.
    On,
    /// The compute fallback on any adapter (reference and test path).
    Compute,
    /// The screen-space trace on any adapter.
    Screen,
}

impl RtShadowRequest {
    /// Every request.
    pub const ALL: [Self; 4] = [Self::Off, Self::On, Self::Compute, Self::Screen];

    /// The request a name stands for: `off`, `on`, `compute` or `screen`. Case-insensitive;
    /// surrounding whitespace is ignored. `None` for anything else.
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "on" => Some(Self::On),
            "compute" => Some(Self::Compute),
            "screen" => Some(Self::Screen),
            _ => None,
        }
    }

    /// `"off"`, `"on"`, `"compute"` or `"screen"` (agent protocol).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Compute => "compute",
            Self::Screen => "screen",
        }
    }

    /// What this request asks of the device: only [`Self::On`] wants ray queries, so a device
    /// for any other profile carries no experimental feature.
    #[must_use]
    pub fn device_request(self) -> HwrtRequest {
        if self == Self::On {
            HwrtRequest::On
        } else {
            HwrtRequest::Off
        }
    }
}

/// A trace shader: its tracer's file name, the whole module's WGSL, and whether it needs ray
/// queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceShader {
    /// The tracer's file name under `crates/regia_render/src/hwrt/`.
    pub file: &'static str,
    /// The WGSL source of the whole pass (`sun_trace_main.wgsl` with the tracer).
    pub source: &'static str,
    /// The module needs `EXPERIMENTAL_RAY_QUERY` (`enable wgpu_ray_query`).
    pub needs_ray_query: bool,
}

/// Which tracer feeds the sun's visibility, and so which shaders the frame compiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SunShadowVariant {
    /// The cascaded shadow map (no def, no trace).
    #[default]
    ShadowMap,
    /// Hardware ray queries against a top-level acceleration structure.
    RayQuery,
    /// The compute BVH trace.
    ComputeFallback,
    /// The march through the depth buffer, on top of the shadow map.
    ScreenTrace,
}

impl SunShadowVariant {
    /// Every variant, in key order.
    pub const ALL: [Self; 4] = [
        Self::ShadowMap,
        Self::RayQuery,
        Self::ComputeFallback,
        Self::ScreenTrace,
    ];

    /// `"shadow_map"`, `"ray_query"`, `"compute_fallback"` or `"screen_trace"` (agent protocol).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ShadowMap => "shadow_map",
            Self::RayQuery => ShadowRayPath::RayQuery.as_str(),
            Self::ComputeFallback => ShadowRayPath::ComputeFallback.as_str(),
            Self::ScreenTrace => "screen_trace",
        }
    }

    /// The variant traces against scene geometry (either tracer), so its visibility replaces the
    /// shadow map on opaque surfaces.
    #[must_use]
    pub fn traces_rays(self) -> bool {
        matches!(self, Self::RayQuery | Self::ComputeFallback)
    }

    /// A pass writes the screen visibility texture this frame, in place of contact shadows.
    #[must_use]
    pub fn writes_visibility(self) -> bool {
        self != Self::ShadowMap
    }

    /// Which forward shader the variant draws with: `true` is the [`SUN_SHADOW_RT_DEF`] module.
    /// This is the bit in the forward pipeline keys: variants that agree on it share pipelines.
    #[must_use]
    pub fn forward_key_bit(self) -> bool {
        self.traces_rays()
    }

    /// The forward-shader defs of this variant, by name (all boolean `true`).
    #[must_use]
    pub fn forward_def_names(self) -> &'static [&'static str] {
        if self.traces_rays() {
            &[SUN_SHADOW_RT_DEF]
        } else {
            &[]
        }
    }

    /// Adds this variant's forward-shader defs to `defs` (a forward pipeline's def set).
    pub fn add_forward_defs(self, defs: &mut HashMap<String, ShaderDefValue>) {
        for name in self.forward_def_names() {
            defs.insert((*name).to_string(), ShaderDefValue::Bool(true));
        }
    }

    /// This variant's forward-shader defs alone.
    #[must_use]
    pub fn forward_shader_defs(self) -> HashMap<String, ShaderDefValue> {
        let mut defs = HashMap::new();
        self.add_forward_defs(&mut defs);
        defs
    }

    /// The trace shader this variant dispatches before the forward pass (`None` for the shadow
    /// map).
    #[must_use]
    pub fn trace_shader(self) -> Option<TraceShader> {
        match self {
            Self::ShadowMap => None,
            Self::RayQuery => Some(TraceShader {
                file: "sun_trace_ray_query.wgsl",
                source: SUN_TRACE_RAY_QUERY_WGSL,
                needs_ray_query: true,
            }),
            Self::ComputeFallback => Some(TraceShader {
                file: "sun_trace_bvh.wgsl",
                source: SUN_TRACE_BVH_WGSL,
                needs_ray_query: false,
            }),
            Self::ScreenTrace => Some(TraceShader {
                file: "sun_trace_screen.wgsl",
                source: SUN_TRACE_SCREEN_WGSL,
                needs_ray_query: false,
            }),
        }
    }

    /// Device features this variant needs (`EXPERIMENTAL_RAY_QUERY` for [`Self::RayQuery`]).
    #[must_use]
    pub fn required_features(self) -> wgpu::Features {
        match self {
            Self::RayQuery => wgpu::Features::EXPERIMENTAL_RAY_QUERY,
            Self::ShadowMap | Self::ComputeFallback | Self::ScreenTrace => wgpu::Features::empty(),
        }
    }

    /// Whether a device with `features` can run this variant.
    #[must_use]
    pub fn runs_on(self, features: wgpu::Features) -> bool {
        features.contains(self.required_features())
    }

    /// The variant's id in reports (two bits, `0..=3`). The shadow map is 0. Pipelines key on
    /// [`Self::forward_key_bit`].
    #[must_use]
    pub fn key_bits(self) -> u8 {
        match self {
            Self::ShadowMap => 0,
            Self::RayQuery => 1,
            Self::ComputeFallback => 2,
            Self::ScreenTrace => 3,
        }
    }

    /// The variant of [`Self::key_bits`] `bits`; `None` for any other value.
    #[must_use]
    pub fn from_key_bits(bits: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.key_bits() == bits)
    }
}

/// Why a frame draws its sun-shadow variant. [`Self::code`] is part of the agent protocol
/// (`render.hwrt`, `book/agents/hwrt.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SunShadowReason {
    /// [`crate::RenderProfile::sun_shadow_trace`] is off.
    NotRequested,
    /// The compute fallback was asked for explicitly.
    ComputeRequested,
    /// The screen-space trace was asked for explicitly.
    ScreenRequested,
    /// No render device has been created in this process yet.
    NoDevice,
    /// Ray queries were asked for and the device has none (its `render.hwrt` reason says why),
    /// so the frame takes the screen-space trace.
    ScreenFallback,
    /// Ray queries were asked for, and the frame has a shared draw that hands the trace no
    /// casters ([`crate::SharedPassDraw::trace_casters`] is `None`): they would not be in the
    /// traced scene, so the frame takes the screen-space trace on the shadow map instead of
    /// losing their shadows. A draw with a trace proxy (the terrain, ADR-2310) does not.
    ScreenSharedCasters,
    /// Ray queries are enabled on the device.
    RayQuery,
    /// The frame has no single-sample depth prepass for the trace to start from.
    NoDepth,
    /// The sun casts no shadow this frame.
    NoSun,
    /// The frame draws the lite forward shader, which has no visibility texture.
    ForwardLite,
    /// A texture view or an eye: only the main view traces.
    SecondaryView,
    /// The traced scene was refused (over the device's limits, say): the frame keeps the shadow
    /// map until the request changes.
    AccelFailed,
}

impl SunShadowReason {
    /// Every reason.
    pub const ALL: [Self; 12] = [
        Self::NotRequested,
        Self::ComputeRequested,
        Self::ScreenRequested,
        Self::NoDevice,
        Self::ScreenFallback,
        Self::ScreenSharedCasters,
        Self::RayQuery,
        Self::NoDepth,
        Self::NoSun,
        Self::ForwardLite,
        Self::SecondaryView,
        Self::AccelFailed,
    ];

    /// Stable lowercase code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::NotRequested => "rt_shadows_not_requested",
            Self::ComputeRequested => "rt_shadows_compute_requested",
            Self::ScreenRequested => "rt_shadows_screen_requested",
            Self::NoDevice => "rt_shadows_no_device",
            Self::ScreenFallback => "rt_shadows_screen_fallback",
            Self::ScreenSharedCasters => "rt_shadows_screen_shared_casters",
            Self::RayQuery => "rt_shadows_ray_query",
            Self::NoDepth => "rt_shadows_no_depth",
            Self::NoSun => "rt_shadows_no_sun",
            Self::ForwardLite => "rt_shadows_forward_lite",
            Self::SecondaryView => "rt_shadows_secondary_view",
            Self::AccelFailed => "rt_shadows_accel_failed",
        }
    }

    /// One sentence for logs and the agent.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::NotRequested => "ray-traced sun shadows are off in this profile",
            Self::ComputeRequested => "the profile asks for the compute fallback",
            Self::ScreenRequested => "the profile asks for the screen-space trace",
            Self::NoDevice => "no render device has been created in this process yet",
            Self::ScreenFallback => {
                "the device has no hardware ray queries (see the hwrt reason); the screen-space trace runs on the shadow map"
            }
            Self::ScreenSharedCasters => {
                "a shared draw has no trace casters; the screen-space trace runs on the shadow map"
            }
            Self::RayQuery => "sun shadows trace with hardware ray queries",
            Self::NoDepth => "the frame has no single-sample depth prepass for the trace to start from",
            Self::NoSun => "the sun casts no shadow this frame",
            Self::ForwardLite => "the lite forward shader has no visibility texture",
            Self::SecondaryView => "only the main view traces",
            Self::AccelFailed => {
                "the traced scene was refused; the shadow map stays until the request changes"
            }
        }
    }
}

/// The outcome of [`select_sun_shadow_variant`] or [`frame_sun_shadow_selection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SunShadowSelection {
    /// What the profile asked for.
    pub request: RtShadowRequest,
    /// The render preset the choice was made for (`None`: a custom profile, or unknown).
    pub preset: Option<RenderProfilePreset>,
    /// The chosen variant.
    pub variant: SunShadowVariant,
    /// Why.
    pub reason: SunShadowReason,
}

impl Default for SunShadowSelection {
    fn default() -> Self {
        Self {
            request: RtShadowRequest::Off,
            preset: None,
            variant: SunShadowVariant::ShadowMap,
            reason: SunShadowReason::NotRequested,
        }
    }
}

/// Chooses the sun-shadow variant for what the profile asks and what the device has: `report` is
/// the device's ([`super::last_device_report`]), `None` before the first device. The order: not
/// requested, then an explicit compute or screen request (any device), then no device; otherwise
/// ray queries where the device has them and the screen-space trace where it has none. A request
/// for ray queries never falls back to the compute tracer, which walks one BVH over world
/// triangles and is not tuned for a frame budget.
#[must_use]
pub fn select_sun_shadow_variant(
    request: RtShadowRequest,
    report: Option<&HwrtReport>,
    preset: Option<RenderProfilePreset>,
) -> SunShadowSelection {
    let (variant, reason) = match request {
        RtShadowRequest::Off => (SunShadowVariant::ShadowMap, SunShadowReason::NotRequested),
        RtShadowRequest::Compute => (
            SunShadowVariant::ComputeFallback,
            SunShadowReason::ComputeRequested,
        ),
        RtShadowRequest::Screen => (SunShadowVariant::ScreenTrace, SunShadowReason::ScreenRequested),
        RtShadowRequest::On => match report {
            None => (SunShadowVariant::ShadowMap, SunShadowReason::NoDevice),
            Some(r) if r.shadow_path() == ShadowRayPath::RayQuery => {
                (SunShadowVariant::RayQuery, SunShadowReason::RayQuery)
            }
            Some(_) => (SunShadowVariant::ScreenTrace, SunShadowReason::ScreenFallback),
        },
    };
    SunShadowSelection {
        request,
        preset,
        variant,
        reason,
    }
}

/// What one frame offers the trace ([`frame_sun_shadow_selection`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTraceSupport {
    /// The device has `EXPERIMENTAL_RAY_QUERY`.
    pub device_ray_query: bool,
    /// The frame has a single-sample depth prepass.
    pub single_sample_depth: bool,
    /// The sun casts shadows this frame.
    pub sun_shadows: bool,
    /// The frame draws the full forward shader (not the lite one).
    pub full_forward: bool,
    /// The frame is the main view.
    pub main_view: bool,
    /// The frame has a shared draw whose casters cannot enter the traced scene (it gives no
    /// [`crate::SharedPassDraw::trace_casters`]).
    pub shared_casters: bool,
    /// The traced scene was refused earlier (`rt_shadows_accel_failed`).
    pub accel_failed: bool,
}

/// The variant one frame draws: [`select_sun_shadow_variant`]'s order for this frame's device, then
/// whatever keeps this frame on the shadow map. An off request returns before any of it, so a
/// default frame reads nothing of the frame.
#[must_use]
pub fn frame_sun_shadow_selection(
    request: RtShadowRequest,
    support: FrameTraceSupport,
    preset: Option<RenderProfilePreset>,
) -> SunShadowSelection {
    let (variant, reason) = match request {
        RtShadowRequest::Off => (SunShadowVariant::ShadowMap, SunShadowReason::NotRequested),
        _ if !support.main_view => (SunShadowVariant::ShadowMap, SunShadowReason::SecondaryView),
        _ if !support.sun_shadows => (SunShadowVariant::ShadowMap, SunShadowReason::NoSun),
        _ if !support.single_sample_depth => (SunShadowVariant::ShadowMap, SunShadowReason::NoDepth),
        _ if !support.full_forward => (SunShadowVariant::ShadowMap, SunShadowReason::ForwardLite),
        RtShadowRequest::Screen => (SunShadowVariant::ScreenTrace, SunShadowReason::ScreenRequested),
        RtShadowRequest::On if !support.device_ray_query => {
            (SunShadowVariant::ScreenTrace, SunShadowReason::ScreenFallback)
        }
        RtShadowRequest::On if support.shared_casters => (
            SunShadowVariant::ScreenTrace,
            SunShadowReason::ScreenSharedCasters,
        ),
        _ if support.accel_failed => (SunShadowVariant::ShadowMap, SunShadowReason::AccelFailed),
        RtShadowRequest::Compute => (
            SunShadowVariant::ComputeFallback,
            SunShadowReason::ComputeRequested,
        ),
        RtShadowRequest::On => (SunShadowVariant::RayQuery, SunShadowReason::RayQuery),
    };
    SunShadowSelection {
        request,
        preset,
        variant,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwrt::{AccelLimits, RayTracingProbe};

    fn probe(ray_query: bool) -> RayTracingProbe {
        RayTracingProbe {
            backend: wgpu::Backend::Vulkan,
            ray_query,
            vertex_return: false,
            limits: AccelLimits {
                max_blas_primitive_count: 1 << 20,
                max_blas_geometry_count: 64,
                max_tlas_instance_count: 1 << 16,
                max_acceleration_structures_per_shader_stage: 8,
            },
        }
    }

    fn enabled_report() -> HwrtReport {
        let p = probe(true);
        let d = p.decide(HwrtRequest::On, false);
        HwrtReport::new(HwrtRequest::On, p, d, wgpu::Features::EXPERIMENTAL_RAY_QUERY)
    }

    fn refused_report() -> HwrtReport {
        let p = probe(false);
        let d = p.decide(HwrtRequest::On, false);
        HwrtReport::new(HwrtRequest::On, p, d, wgpu::Features::empty())
    }

    fn full_support() -> FrameTraceSupport {
        FrameTraceSupport {
            device_ray_query: true,
            single_sample_depth: true,
            sun_shadows: true,
            full_forward: true,
            main_view: true,
            shared_casters: false,
            accel_failed: false,
        }
    }

    #[test]
    fn the_request_names_round_trip_and_nothing_else_parses() {
        for request in RtShadowRequest::ALL {
            assert_eq!(RtShadowRequest::from_name(request.as_str()), Some(request));
        }
        assert_eq!(RtShadowRequest::from_name(" ON "), Some(RtShadowRequest::On));
        for bad in ["", "1", "yes", "auto", "ray_query", "fallback"] {
            assert_eq!(RtShadowRequest::from_name(bad), None, "{bad:?}");
        }
        assert_eq!(RtShadowRequest::default(), RtShadowRequest::Off);
    }

    /// Only asking for hardware ray queries touches the device request, so a profile with the
    /// knob off, or on a path that needs no ray queries, leaves the device as it was.
    #[test]
    fn only_on_asks_the_device_for_ray_queries() {
        for request in RtShadowRequest::ALL {
            let want = request == RtShadowRequest::On;
            assert_eq!(request.device_request() == HwrtRequest::On, want, "{request:?}");
        }
    }

    #[test]
    fn off_keeps_the_shadow_map_on_every_device_and_preset() {
        for report in [None, Some(enabled_report()), Some(refused_report())] {
            for preset in RenderProfilePreset::ALL.iter().copied().map(Some).chain([None]) {
                let s = select_sun_shadow_variant(RtShadowRequest::Off, report.as_ref(), preset);
                assert_eq!(s.variant, SunShadowVariant::ShadowMap);
                assert_eq!(s.reason, SunShadowReason::NotRequested);
            }
        }
    }

    #[test]
    fn on_takes_ray_queries_and_falls_back_to_the_screen_trace() {
        let r = enabled_report();
        let s = select_sun_shadow_variant(RtShadowRequest::On, Some(&r), None);
        assert_eq!(
            (s.variant, s.reason),
            (SunShadowVariant::RayQuery, SunShadowReason::RayQuery)
        );
        let r = refused_report();
        let s = select_sun_shadow_variant(RtShadowRequest::On, Some(&r), None);
        assert_eq!(
            (s.variant, s.reason),
            (SunShadowVariant::ScreenTrace, SunShadowReason::ScreenFallback)
        );
        let s = select_sun_shadow_variant(RtShadowRequest::On, None, None);
        assert_eq!(
            (s.variant, s.reason),
            (SunShadowVariant::ShadowMap, SunShadowReason::NoDevice)
        );
    }

    #[test]
    fn compute_and_screen_run_on_any_device() {
        for report in [None, Some(enabled_report()), Some(refused_report())] {
            let s = select_sun_shadow_variant(RtShadowRequest::Compute, report.as_ref(), None);
            assert_eq!(
                (s.variant, s.reason),
                (
                    SunShadowVariant::ComputeFallback,
                    SunShadowReason::ComputeRequested
                )
            );
            assert!(s.variant.runs_on(wgpu::Features::empty()));
            let s = select_sun_shadow_variant(RtShadowRequest::Screen, report.as_ref(), None);
            assert_eq!(
                (s.variant, s.reason),
                (SunShadowVariant::ScreenTrace, SunShadowReason::ScreenRequested)
            );
            assert!(s.variant.runs_on(wgpu::Features::empty()));
        }
    }

    /// An off request stays on the shadow map whatever the frame offers: the default frame
    /// depends on nothing here.
    #[test]
    fn a_frame_with_the_knob_off_never_traces() {
        for main_view in [false, true] {
            for device_ray_query in [false, true] {
                let support = FrameTraceSupport {
                    main_view,
                    device_ray_query,
                    ..full_support()
                };
                let s = frame_sun_shadow_selection(RtShadowRequest::Off, support, None);
                assert_eq!(
                    (s.variant, s.reason),
                    (SunShadowVariant::ShadowMap, SunShadowReason::NotRequested)
                );
            }
        }
    }

    /// Each thing a frame can lack keeps it on the shadow map and names itself, for every
    /// request that would have traced.
    #[test]
    fn a_frame_that_cannot_trace_says_why() {
        let cases = [
            (
                FrameTraceSupport {
                    main_view: false,
                    ..full_support()
                },
                SunShadowReason::SecondaryView,
            ),
            (
                FrameTraceSupport {
                    sun_shadows: false,
                    ..full_support()
                },
                SunShadowReason::NoSun,
            ),
            (
                FrameTraceSupport {
                    single_sample_depth: false,
                    ..full_support()
                },
                SunShadowReason::NoDepth,
            ),
            (
                FrameTraceSupport {
                    full_forward: false,
                    ..full_support()
                },
                SunShadowReason::ForwardLite,
            ),
        ];
        for request in [
            RtShadowRequest::On,
            RtShadowRequest::Compute,
            RtShadowRequest::Screen,
        ] {
            for (support, reason) in cases {
                let s = frame_sun_shadow_selection(request, support, None);
                assert_eq!((s.variant, s.reason), (SunShadowVariant::ShadowMap, reason));
            }
        }
    }

    #[test]
    fn a_frame_takes_the_variant_its_device_runs() {
        let on = |device_ray_query, accel_failed| {
            let support = FrameTraceSupport {
                device_ray_query,
                accel_failed,
                ..full_support()
            };
            let s = frame_sun_shadow_selection(RtShadowRequest::On, support, None);
            (s.variant, s.reason)
        };
        assert_eq!(
            on(true, false),
            (SunShadowVariant::RayQuery, SunShadowReason::RayQuery)
        );
        assert_eq!(
            on(false, false),
            (SunShadowVariant::ScreenTrace, SunShadowReason::ScreenFallback)
        );
        assert_eq!(
            on(true, true),
            (SunShadowVariant::ShadowMap, SunShadowReason::AccelFailed)
        );
    }

    /// A shared draw's casters are not in the traced scene: `on` keeps the shadow map under the
    /// screen trace rather than lose their shadows; an explicit reference request still traces.
    #[test]
    fn a_shared_draw_keeps_the_shadow_map_under_on() {
        let support = FrameTraceSupport {
            shared_casters: true,
            ..full_support()
        };
        let s = frame_sun_shadow_selection(RtShadowRequest::On, support, None);
        assert_eq!(
            (s.variant, s.reason),
            (
                SunShadowVariant::ScreenTrace,
                SunShadowReason::ScreenSharedCasters
            )
        );
        let s = frame_sun_shadow_selection(RtShadowRequest::Compute, support, None);
        assert_eq!(s.variant, SunShadowVariant::ComputeFallback);
    }

    #[test]
    fn each_variant_names_its_defs_trace_shader_and_features() {
        let map = SunShadowVariant::ShadowMap;
        assert!(map.forward_def_names().is_empty());
        assert!(map.forward_shader_defs().is_empty());
        assert!(map.trace_shader().is_none());
        assert!(map.required_features().is_empty());
        assert!(!map.writes_visibility() && !map.forward_key_bit());

        let rq = SunShadowVariant::RayQuery;
        assert_eq!(rq.forward_def_names(), &[SUN_SHADOW_RT_DEF]);
        let trace = rq.trace_shader().expect("ray-query trace");
        assert!(trace.needs_ray_query);
        assert_eq!(trace.source, SUN_TRACE_RAY_QUERY_WGSL);
        assert!(trace.source.starts_with("enable wgpu_ray_query"));
        assert_eq!(rq.required_features(), wgpu::Features::EXPERIMENTAL_RAY_QUERY);
        assert!(!rq.runs_on(wgpu::Features::empty()));
        assert!(rq.runs_on(wgpu::Features::EXPERIMENTAL_RAY_QUERY));
        assert!(rq.writes_visibility() && rq.forward_key_bit());

        let cf = SunShadowVariant::ComputeFallback;
        assert_eq!(cf.forward_def_names(), &[SUN_SHADOW_RT_DEF]);
        let trace = cf.trace_shader().expect("fallback trace");
        assert!(!trace.needs_ray_query);
        assert_eq!(trace.source, SUN_TRACE_BVH_WGSL);
        assert!(!trace.source.contains("wgpu_ray_query"));
        assert!(cf.writes_visibility() && cf.forward_key_bit());

        // The screen trace keeps the shadow-map forward shader.
        let screen = SunShadowVariant::ScreenTrace;
        assert!(screen.forward_def_names().is_empty());
        let trace = screen.trace_shader().expect("screen trace");
        assert!(!trace.needs_ray_query);
        assert_eq!(trace.source, SUN_TRACE_SCREEN_WGSL);
        assert!(screen.runs_on(wgpu::Features::empty()));
        assert!(screen.writes_visibility() && !screen.forward_key_bit());

        // Adding a variant's defs keeps the pipeline's other defs.
        let mut defs = HashMap::from([("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true))]);
        rq.add_forward_defs(&mut defs);
        assert_eq!(defs.len(), 2);
        assert_eq!(defs.get(SUN_SHADOW_RT_DEF), Some(&ShaderDefValue::Bool(true)));
        // The tracers exist under the names the variants give, and each is in its pass.
        for v in SunShadowVariant::ALL {
            if let Some(t) = v.trace_shader() {
                let path = format!("{}/src/hwrt/{}", env!("CARGO_MANIFEST_DIR"), t.file);
                let on_disk = std::fs::read_to_string(&path).expect("tracer on disk");
                assert!(
                    t.source
                        .replace("\r\n", "\n")
                        .contains(&on_disk.replace("\r\n", "\n")),
                    "{path}"
                );
            }
        }
    }

    #[test]
    fn key_bits_are_two_bits_unique_and_round_trip() {
        let mut seen = Vec::new();
        for v in SunShadowVariant::ALL {
            let bits = v.key_bits();
            assert!(bits < 4, "{v:?}");
            assert!(!seen.contains(&bits), "{v:?}");
            seen.push(bits);
            assert_eq!(SunShadowVariant::from_key_bits(bits), Some(v));
        }
        assert_eq!(SunShadowVariant::ShadowMap.key_bits(), 0);
        assert_eq!(SunShadowVariant::default(), SunShadowVariant::ShadowMap);
        assert_eq!(SunShadowVariant::from_key_bits(4), None);
    }

    #[test]
    fn codes_are_lowercase_protocol_codes_and_unique() {
        let ok = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        };
        let mut codes: Vec<&str> = SunShadowReason::ALL.iter().map(|r| r.code()).collect();
        for r in SunShadowReason::ALL {
            assert!(ok(r.code()), "{}", r.code());
            assert!(!r.message().is_empty());
        }
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), SunShadowReason::ALL.len());
        for v in SunShadowVariant::ALL {
            assert!(ok(v.as_str()), "{}", v.as_str());
        }
        for r in RtShadowRequest::ALL {
            assert!(ok(r.as_str()));
        }
    }

    /// The forward shader composes and validates under every variant's defs, and the
    /// ray-traced def changes the module (the `#ifdef` exists and its name matches).
    #[test]
    fn the_forward_shader_compiles_every_variant() {
        let forward = include_str!("../../shaders/lab_forward.wgsl");
        let mut modules = Vec::new();
        for v in SunShadowVariant::ALL {
            let mut defs = v.forward_shader_defs();
            defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
            let module = crate::shader_lib::compose_source(forward, "lab_forward.wgsl", defs)
                .unwrap_or_else(|e| panic!("{v:?}: {e}"));
            crate::shader_reload::naga_validate(&module).unwrap_or_else(|e| panic!("{v:?}: {e}"));
            modules.push(format!("{module:?}"));
        }
        assert_ne!(
            modules[0], modules[1],
            "SUN_SHADOW_RT must change the forward shader"
        );
        assert_eq!(
            modules[1], modules[2],
            "both ray-traced variants compile the same forward shader"
        );
        assert_eq!(
            modules[0], modules[3],
            "the screen trace keeps the shadow map's forward shader"
        );
        // Variants share forward pipelines exactly when they share the module.
        for (a, va) in SunShadowVariant::ALL.iter().enumerate() {
            for (b, vb) in SunShadowVariant::ALL.iter().enumerate() {
                assert_eq!(
                    va.forward_key_bit() == vb.forward_key_bit(),
                    modules[a] == modules[b]
                );
            }
        }
    }
}
