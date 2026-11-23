// SPDX-License-Identifier: MPL-2.0
//! Every adapter's hardware ray-tracing offer, without creating a device (`regia doctor`,
//! ADR-1149).
//!
//! `render.hwrt` describes the device a running app created. Before a game runs, the question
//! is what each GPU in the machine would give: [`probe_adapters`] lists the adapters a device
//! could select (the same numbering as `REGIA_GPU_INDEX` and [`crate::list_adapters`]) with
//! the [`RayTracingProbe`] of each, and [`AdapterRayTracing::decide`] says what a device on that
//! adapter would get.

use super::probe::{HwrtDecision, HwrtRequest, RayTracingProbe};

/// One adapter's ray-tracing offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterRayTracing {
    /// Index for `REGIA_GPU_INDEX`.
    pub index: usize,
    /// Adapter name.
    pub name: String,
    /// Discrete, integrated, software, ...
    pub device_type: wgpu::DeviceType,
    /// Driver name.
    pub driver: String,
    /// Driver version details.
    pub driver_info: String,
    /// Ray queries, vertex return and acceleration-structure limits (its `backend` is the
    /// adapter's backend).
    pub probe: RayTracingProbe,
}

impl AdapterRayTracing {
    /// What a full-feature device (no min-spec profile) on this adapter would get for
    /// `request`.
    #[must_use]
    pub fn decide(&self, request: HwrtRequest) -> HwrtDecision {
        self.probe.decide(request, false)
    }

    /// The device type in lowercase words (`"discrete"`, `"integrated"`, `"virtual"`, `"cpu"`,
    /// `"other"`).
    #[must_use]
    pub fn device_type_str(&self) -> &'static str {
        match self.device_type {
            wgpu::DeviceType::DiscreteGpu => "discrete",
            wgpu::DeviceType::IntegratedGpu => "integrated",
            wgpu::DeviceType::VirtualGpu => "virtual",
            wgpu::DeviceType::Cpu => "cpu",
            wgpu::DeviceType::Other => "other",
        }
    }
}

/// Probes every adapter a device could select, in [`crate::list_adapters`] order. Enumerates the
/// adapters of the process's headless instance (created on first use, ADR-7557); creates no
/// device. Empty when the machine has no adapter
/// on Regia Engine's backends ([`crate::instance_backends`]).
#[must_use]
pub fn probe_adapters() -> Vec<AdapterRayTracing> {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = crate::instance_backends();
    // The engine's flags, so this reuses the headless devices' instance instead of creating one
    // of its own while a renderer may be naming objects on another thread (ADR-7557).
    descriptor.flags = crate::device::instance_flags();
    let instance = crate::device::create_instance(descriptor);
    crate::gpu_select::enumerate(&instance)
        .iter()
        .enumerate()
        .map(|(index, adapter)| {
            let info = adapter.get_info();
            AdapterRayTracing {
                index,
                name: info.name,
                device_type: info.device_type,
                driver: info.driver,
                driver_info: info.driver_info,
                probe: RayTracingProbe::from_adapter(adapter),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwrt::{AccelLimits, HwrtReason};

    fn adapter(ray_query: bool, limits: AccelLimits) -> AdapterRayTracing {
        AdapterRayTracing {
            index: 0,
            name: "test adapter".into(),
            device_type: wgpu::DeviceType::DiscreteGpu,
            driver: "test".into(),
            driver_info: String::new(),
            probe: RayTracingProbe {
                backend: wgpu::Backend::Vulkan,
                ray_query,
                vertex_return: false,
                limits,
            },
        }
    }

    fn usable() -> AccelLimits {
        AccelLimits {
            max_blas_primitive_count: 1 << 20,
            max_blas_geometry_count: 64,
            max_tlas_instance_count: 1 << 16,
            max_acceleration_structures_per_shader_stage: 8,
        }
    }

    #[test]
    fn the_decision_ignores_min_spec_and_follows_the_probe() {
        let a = adapter(true, usable());
        assert_eq!(a.decide(HwrtRequest::On).reason, HwrtReason::Enabled);
        assert_eq!(a.decide(HwrtRequest::Off).reason, HwrtReason::NotRequested);
        let a = adapter(false, usable());
        assert_eq!(a.decide(HwrtRequest::On).reason, HwrtReason::NoAdapterRayQuery);
        let a = adapter(true, AccelLimits::default());
        assert_eq!(a.decide(HwrtRequest::On).reason, HwrtReason::ZeroAccelLimits);
    }

    #[test]
    fn device_types_have_lowercase_names() {
        let mut a = adapter(false, usable());
        for (t, name) in [
            (wgpu::DeviceType::DiscreteGpu, "discrete"),
            (wgpu::DeviceType::IntegratedGpu, "integrated"),
            (wgpu::DeviceType::VirtualGpu, "virtual"),
            (wgpu::DeviceType::Cpu, "cpu"),
            (wgpu::DeviceType::Other, "other"),
        ] {
            a.device_type = t;
            assert_eq!(a.device_type_str(), name);
        }
    }
}
