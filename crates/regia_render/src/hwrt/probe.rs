// SPDX-License-Identifier: MPL-2.0
//! Ray-query capability probing and the opt-in device request (ADR-515).
//!
//! The decision is a pure function of what the adapter reports and what the host asked for, so
//! every branch is unit-tested without ray-tracing hardware. [`crate::RenderDevice`] calls
//! `plan_device_request` before it creates the device and records a [`HwrtReport`] after,
//! which `render.hwrt` (agent protocol) reads through [`last_device_report`]. The request comes
//! from the render profile ([`crate::RenderProfile::sun_shadow_trace`] or
//! [`crate::RenderProfile::restir_gi`], ADR-2035 and ADR-3349): the renderer plugin hands it over
//! before the first window's device ([`set_device_request`]).

use std::sync::Mutex;

/// Whether the host asked for hardware ray queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HwrtRequest {
    /// Not asked for (the default): the device is created exactly as without this module.
    #[default]
    Off,
    /// Asked for: requested from the device when the adapter has them.
    On,
}

impl HwrtRequest {
    /// `"off"` or `"on"` (agent protocol).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }
}

/// Acceleration-structure limits of an adapter or device (`wgpu::Limits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccelLimits {
    /// Triangles (or boxes) one bottom-level structure may hold.
    pub max_blas_primitive_count: u32,
    /// Geometry descriptors one bottom-level structure may hold.
    pub max_blas_geometry_count: u32,
    /// Instances one top-level structure may hold.
    pub max_tlas_instance_count: u32,
    /// Acceleration structures one shader stage may bind.
    pub max_acceleration_structures_per_shader_stage: u32,
}

impl AccelLimits {
    /// The acceleration-structure part of `limits`.
    #[must_use]
    pub fn from_limits(limits: &wgpu::Limits) -> Self {
        Self {
            max_blas_primitive_count: limits.max_blas_primitive_count,
            max_blas_geometry_count: limits.max_blas_geometry_count,
            max_tlas_instance_count: limits.max_tlas_instance_count,
            max_acceleration_structures_per_shader_stage: limits.max_acceleration_structures_per_shader_stage,
        }
    }

    /// Every limit is non-zero: one triangle mesh, one instance and one bound structure fit.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.max_blas_primitive_count > 0
            && self.max_blas_geometry_count > 0
            && self.max_tlas_instance_count > 0
            && self.max_acceleration_structures_per_shader_stage > 0
    }
}

/// What an adapter offers for hardware ray tracing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RayTracingProbe {
    /// Backend of the adapter (wgpu 30 has ray queries on Vulkan only).
    pub backend: wgpu::Backend,
    /// The adapter has `EXPERIMENTAL_RAY_QUERY`.
    pub ray_query: bool,
    /// The adapter has `EXPERIMENTAL_RAY_HIT_VERTEX_RETURN` (hit triangle positions in shaders).
    pub vertex_return: bool,
    /// The adapter's acceleration-structure limits.
    pub limits: AccelLimits,
}

impl RayTracingProbe {
    /// A probe from an adapter's backend, features and limits.
    #[must_use]
    pub fn from_parts(backend: wgpu::Backend, features: wgpu::Features, limits: &wgpu::Limits) -> Self {
        Self {
            backend,
            ray_query: features.contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY),
            vertex_return: features.contains(wgpu::Features::EXPERIMENTAL_RAY_HIT_VERTEX_RETURN),
            limits: AccelLimits::from_limits(limits),
        }
    }

    /// Probes `adapter`.
    #[must_use]
    pub fn from_adapter(adapter: &wgpu::Adapter) -> Self {
        Self::from_parts(adapter.get_info().backend, adapter.features(), &adapter.limits())
    }

    /// What to request from the device for `request`. `minspec` is true when the device is
    /// clamped to a minimum-spec profile ([`crate::minspec::MinspecProfile`]): no min-spec tier
    /// has ray queries, so the profile never gets them.
    #[must_use]
    pub fn decide(&self, request: HwrtRequest, minspec: bool) -> HwrtDecision {
        let reason = if request == HwrtRequest::Off {
            HwrtReason::NotRequested
        } else if minspec {
            HwrtReason::MinspecProfile
        } else if !self.ray_query {
            HwrtReason::NoAdapterRayQuery
        } else if !self.limits.is_usable() {
            HwrtReason::ZeroAccelLimits
        } else {
            HwrtReason::Enabled
        };
        let features = if reason == HwrtReason::Enabled {
            let mut features = wgpu::Features::EXPERIMENTAL_RAY_QUERY;
            if self.vertex_return {
                features |= wgpu::Features::EXPERIMENTAL_RAY_HIT_VERTEX_RETURN;
            }
            features
        } else {
            wgpu::Features::empty()
        };
        HwrtDecision { reason, features }
    }
}

/// Which shadow-ray tracer a device runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowRayPath {
    /// Hardware ray queries against a top-level acceleration structure.
    RayQuery,
    /// The compute fallback ([`crate::hwrt::ShadowFallbackPass`]): a BVH in storage buffers.
    ComputeFallback,
}

impl ShadowRayPath {
    /// `"ray_query"` or `"compute_fallback"` (agent protocol).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RayQuery => "ray_query",
            Self::ComputeFallback => "compute_fallback",
        }
    }
}

/// Why a device has (or lacks) hardware ray queries. [`Self::code`] is part of the agent
/// protocol (`render.hwrt`, `book/agents/hwrt.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwrtReason {
    /// Requested, offered and enabled on the device.
    Enabled,
    /// The render profile does not ask for ray queries.
    NotRequested,
    /// The device is clamped to a minimum-spec profile.
    MinspecProfile,
    /// The adapter has no `EXPERIMENTAL_RAY_QUERY` (any backend but Vulkan, or a driver
    /// without `VK_KHR_ray_query`).
    NoAdapterRayQuery,
    /// The adapter has the feature but reports a zero acceleration-structure limit.
    ZeroAccelLimits,
    /// The feature was requested but the created device does not have it.
    DeviceRefused,
}

impl HwrtReason {
    /// Stable lowercase code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Enabled => "hwrt_enabled",
            Self::NotRequested => "hwrt_not_requested",
            Self::MinspecProfile => "hwrt_minspec_profile",
            Self::NoAdapterRayQuery => "hwrt_no_adapter_ray_query",
            Self::ZeroAccelLimits => "hwrt_zero_accel_limits",
            Self::DeviceRefused => "hwrt_device_refused",
        }
    }

    /// One sentence for logs and the agent.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::Enabled => "hardware ray queries are enabled on this device",
            Self::NotRequested => {
                "the render profile does not ask for hardware ray queries (sun shadows or ReSTIR GI)"
            }
            Self::MinspecProfile => {
                "the device is clamped to a minimum-spec profile, which has no ray queries"
            }
            Self::NoAdapterRayQuery => {
                "the adapter has no ray queries (wgpu 30 offers them on Vulkan with VK_KHR_ray_query only)"
            }
            Self::ZeroAccelLimits => "the adapter offers ray queries but a zero acceleration-structure limit",
            Self::DeviceRefused => "ray queries were requested but the created device does not have them",
        }
    }
}

/// The outcome of [`RayTracingProbe::decide`]: features to add to the device request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HwrtDecision {
    /// Why.
    pub reason: HwrtReason,
    /// Features to request (ray query, plus hit-vertex return where offered, when enabled;
    /// otherwise empty).
    pub features: wgpu::Features,
}

impl HwrtDecision {
    /// The experimental-features token for the device descriptor: enabled only when this
    /// decision requests an experimental feature, so a default device never opts in.
    #[must_use]
    // ADR-515: wgpu 30 makes the experimental opt-in an `unsafe fn`. This is the only place
    // Regia Engine calls it, and only when the render profile asked for ray queries.
    #[allow(unsafe_code)]
    pub fn experimental_features(&self) -> wgpu::ExperimentalFeatures {
        if self.features.intersects(wgpu::Features::all_experimental_mask()) {
            // SAFETY: the token accepts that wgpu's experimental features may still have bugs,
            // undefined behavior included; there is no precondition the caller can check. It
            // is created only for a device the render profile explicitly opted in to ray queries
            // (`RenderProfile::sun_shadow_trace` or `restir_gi`), only the probed ray-query
            // features are requested with it, and only the acceleration/tracing paths (which
            // check the device first) use them.
            unsafe { wgpu::ExperimentalFeatures::enabled() }
        } else {
            wgpu::ExperimentalFeatures::disabled()
        }
    }
}

/// A device's hardware ray-tracing state: what was asked, offered, decided and granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HwrtReport {
    /// What the host asked for.
    pub request: HwrtRequest,
    /// What the adapter offers.
    pub probe: RayTracingProbe,
    /// What was requested from the device.
    pub decision: HwrtDecision,
    /// The created device has `EXPERIMENTAL_RAY_QUERY`.
    pub device_ray_query: bool,
}

impl HwrtReport {
    /// The report for a device created with `decision`, whose features are `device_features`.
    #[must_use]
    pub fn new(
        request: HwrtRequest,
        probe: RayTracingProbe,
        decision: HwrtDecision,
        device_features: wgpu::Features,
    ) -> Self {
        Self {
            request,
            probe,
            decision,
            device_ray_query: device_features.contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY),
        }
    }

    /// Why the device has (or lacks) ray queries.
    #[must_use]
    pub fn reason(&self) -> HwrtReason {
        match self.decision.reason {
            HwrtReason::Enabled if !self.device_ray_query => HwrtReason::DeviceRefused,
            other => other,
        }
    }

    /// The shadow-ray tracer this device runs.
    #[must_use]
    pub fn shadow_path(&self) -> ShadowRayPath {
        if self.reason() == HwrtReason::Enabled {
            ShadowRayPath::RayQuery
        } else {
            ShadowRayPath::ComputeFallback
        }
    }
}

static LAST_REPORT: Mutex<Option<HwrtReport>> = Mutex::new(None);

/// The report of the most recent device [`crate::RenderDevice`] created in this process
/// (`None` before the first, or for devices assembled with `RenderDevice::from_parts`).
#[must_use]
pub fn last_device_report() -> Option<HwrtReport> {
    match LAST_REPORT.lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

/// Records `report` as [`last_device_report`] and logs it.
pub(crate) fn record_device_report(report: HwrtReport) {
    let reason = report.reason();
    if report.request == HwrtRequest::On {
        tracing::info!(
            target: "regia_render",
            code = reason.code(),
            "hardware ray tracing: {} (shadow rays: {})",
            reason.message(),
            report.shadow_path().as_str()
        );
    }
    match LAST_REPORT.lock() {
        Ok(mut guard) => *guard = Some(report),
        Err(poisoned) => *poisoned.into_inner() = Some(report),
    }
}

/// Test hook (`test-hooks`): records `report` as [`last_device_report`] exactly as a device
/// creation does, so a test can overwrite the process-wide report from other threads without
/// creating devices. `regia_test_utils`'s `hwrt_gate` races the run-time gate against it: its
/// earlier sibling threads created 48 real devices for that and livelocked the NVIDIA driver on a
/// loaded RTX 5090 (ADR-7570).
#[cfg(feature = "test-hooks")]
pub fn record_device_report_for_test(report: HwrtReport) {
    record_device_report(report);
}

static DEVICE_REQUEST: Mutex<HwrtRequest> = Mutex::new(HwrtRequest::Off);

/// What the devices this process creates from now on ask for ([`RayTracingProbe::decide`]). The
/// renderer plugin sets it from the render profile before the first window's device; a host that
/// creates its own device sets it first. Off until set.
pub fn set_device_request(request: HwrtRequest) {
    match DEVICE_REQUEST.lock() {
        Ok(mut guard) => *guard = request,
        Err(poisoned) => *poisoned.into_inner() = request,
    }
}

/// The request [`set_device_request`] left.
#[must_use]
pub fn device_request() -> HwrtRequest {
    match DEVICE_REQUEST.lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

/// Probes `adapter` and decides the ray-query part of the device request for `request`.
pub(crate) fn plan_device_request(
    adapter: &wgpu::Adapter,
    minspec: bool,
    request: HwrtRequest,
) -> (HwrtRequest, RayTracingProbe, HwrtDecision) {
    let probe = RayTracingProbe::from_adapter(adapter);
    let decision = probe.decide(request, minspec);
    (request, probe, decision)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt_limits() -> wgpu::Limits {
        wgpu::Limits {
            max_blas_primitive_count: 1 << 28,
            max_blas_geometry_count: 1 << 24,
            max_tlas_instance_count: 1 << 24,
            max_acceleration_structures_per_shader_stage: 16,
            ..wgpu::Limits::default()
        }
    }

    fn rt_probe() -> RayTracingProbe {
        RayTracingProbe::from_parts(
            wgpu::Backend::Vulkan,
            wgpu::Features::EXPERIMENTAL_RAY_QUERY,
            &rt_limits(),
        )
    }

    #[test]
    fn the_device_request_is_off_until_a_profile_sets_it() {
        // The default is what every device of a process without the knob gets.
        assert_eq!(HwrtRequest::default(), HwrtRequest::Off);
    }

    #[test]
    fn the_probe_reads_features_and_limits() {
        let p = rt_probe();
        assert!(p.ray_query);
        assert!(!p.vertex_return);
        assert!(p.limits.is_usable());
        assert_eq!(p.limits.max_acceleration_structures_per_shader_stage, 16);
        let plain = RayTracingProbe::from_parts(
            wgpu::Backend::Dx12,
            wgpu::Features::empty(),
            &wgpu::Limits::default(),
        );
        assert!(!plain.ray_query);
        // wgpu's default limits carry no acceleration structures.
        assert!(!plain.limits.is_usable());
    }

    #[test]
    fn off_is_the_default_and_requests_nothing() {
        let d = rt_probe().decide(HwrtRequest::Off, false);
        assert_eq!(d.reason, HwrtReason::NotRequested);
        assert!(d.features.is_empty());
        assert!(!d.experimental_features().is_enabled());
    }

    #[test]
    fn every_refusal_has_its_reason_in_priority_order() {
        let p = rt_probe();
        assert_eq!(p.decide(HwrtRequest::On, true).reason, HwrtReason::MinspecProfile);
        let no_feature = RayTracingProbe {
            ray_query: false,
            ..p
        };
        assert_eq!(
            no_feature.decide(HwrtRequest::On, false).reason,
            HwrtReason::NoAdapterRayQuery
        );
        let mut zero = p;
        zero.limits.max_tlas_instance_count = 0;
        assert_eq!(
            zero.decide(HwrtRequest::On, false).reason,
            HwrtReason::ZeroAccelLimits
        );
        for refused in [
            p.decide(HwrtRequest::On, true),
            no_feature.decide(HwrtRequest::On, false),
        ] {
            assert!(refused.features.is_empty());
            assert!(!refused.experimental_features().is_enabled());
        }
    }

    #[test]
    fn an_offered_request_enables_ray_queries_and_the_token() {
        let d = rt_probe().decide(HwrtRequest::On, false);
        assert_eq!(d.reason, HwrtReason::Enabled);
        assert_eq!(d.features, wgpu::Features::EXPERIMENTAL_RAY_QUERY);
        assert!(d.experimental_features().is_enabled());
    }

    #[test]
    fn an_offered_vertex_return_is_requested_with_ray_queries() {
        let p = RayTracingProbe::from_parts(
            wgpu::Backend::Vulkan,
            wgpu::Features::EXPERIMENTAL_RAY_QUERY | wgpu::Features::EXPERIMENTAL_RAY_HIT_VERTEX_RETURN,
            &rt_limits(),
        );
        assert!(p.vertex_return);
        let d = p.decide(HwrtRequest::On, false);
        assert!(d.features.contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY));
        assert!(
            d.features
                .contains(wgpu::Features::EXPERIMENTAL_RAY_HIT_VERTEX_RETURN)
        );
    }

    #[test]
    fn the_report_falls_back_when_the_device_refuses() {
        let p = rt_probe();
        let d = p.decide(HwrtRequest::On, false);
        let granted = HwrtReport::new(HwrtRequest::On, p, d, wgpu::Features::EXPERIMENTAL_RAY_QUERY);
        assert_eq!(granted.reason(), HwrtReason::Enabled);
        assert_eq!(granted.shadow_path(), ShadowRayPath::RayQuery);
        let refused = HwrtReport::new(HwrtRequest::On, p, d, wgpu::Features::empty());
        assert_eq!(refused.reason(), HwrtReason::DeviceRefused);
        assert_eq!(refused.shadow_path(), ShadowRayPath::ComputeFallback);
        let off = HwrtReport::new(
            HwrtRequest::Off,
            p,
            p.decide(HwrtRequest::Off, false),
            wgpu::Features::empty(),
        );
        assert_eq!(off.shadow_path(), ShadowRayPath::ComputeFallback);
        assert_eq!(off.reason(), HwrtReason::NotRequested);
    }

    #[test]
    fn reason_codes_are_lowercase_protocol_codes_and_unique() {
        let all = [
            HwrtReason::Enabled,
            HwrtReason::NotRequested,
            HwrtReason::MinspecProfile,
            HwrtReason::NoAdapterRayQuery,
            HwrtReason::ZeroAccelLimits,
            HwrtReason::DeviceRefused,
        ];
        let mut codes: Vec<&str> = all.iter().map(|r| r.code()).collect();
        for code in &codes {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
        for r in all {
            assert!(!r.message().is_empty());
        }
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
        assert_eq!(ShadowRayPath::RayQuery.as_str(), "ray_query");
        assert_eq!(ShadowRayPath::ComputeFallback.as_str(), "compute_fallback");
    }
}
