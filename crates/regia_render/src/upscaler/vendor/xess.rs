// SPDX-License-Identifier: MPL-2.0
//! Intel XeSS SR adapter for the vendor slot (ADR-4656).
//!
//! The `xess` feature uses an external Windows SDK. Missing SDKs and runtimes keep the
//! built-in temporal upscaler active. Existing settings and agent methods select `xess`.
use super::{
    FeatureDesc, FeatureHandle, OptimalSettings, RuntimeLibrary, VendorEvalParams, VendorEvaluate,
    VendorRuntimeApi, VulkanExtensionRequest,
};
use crate::upscaler::plugin::{GpuTarget, UpscaleError, UpscaleFrame, UpscaleTarget, VendorRuntime};
use crate::upscaler::{UpscaleQuality, VendorUpscaler, reason};
use std::{path::PathBuf, sync::Mutex};

/// Whether the external SDK binding was generated for this build.
pub const BINDING_BUILT: bool = binding_built();
const fn binding_built() -> bool {
    #[cfg(feature = "xess")]
    {
        regia_xess::LINKED
    }
    #[cfg(not(feature = "xess"))]
    {
        false
    }
}
/// Runtime discovery and last device initialization result, exposed through the agent protocol.
#[derive(Debug, Clone)]
pub struct XessStatus {
    /// Runtime availability state.
    pub runtime: VendorRuntime,
    /// Whether this build contains the SDK adapter.
    pub binding_built: bool,
    /// SDK bundle version, distinct from the SR runtime version.
    pub sdk_version: &'static str,
    /// SR runtime version, once loaded.
    pub runtime_version: Option<[u16; 3]>,
    /// Absolute runtime path located beside the executable or in `REGIA_XESS_PATH`.
    pub library: Option<PathBuf>,
    /// Last binding or SDK error code.
    pub code: Option<&'static str>,
    /// Requested Vulkan instance extensions.
    pub instance_extensions: Vec<String>,
    /// Requested Vulkan device extensions.
    pub device_extensions: Vec<String>,
    /// How the last initialized feature's temporary heaps were zeroed, `[buffer, texture]`
    /// (ADR-4907, ADR-7820): `host_write`, `driver_zeroed` or `device_fill`
    /// (`regia_xess::HeapZeroing::code` in XeSS builds), or `sdk_allocated` twice when XeSS
    /// fell back to its own allocations. `None` before the first feature.
    pub temp_heaps: Option<[&'static str; 2]>,
}
static STATUS: Mutex<Option<XessStatus>> = Mutex::new(None);
fn status_with<T>(f: impl FnOnce(&mut XessStatus) -> T) -> T {
    let mut guard = STATUS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(|| {
        let search = RuntimeLibrary::current(VendorUpscaler::Xess).locate_for_this_process();
        XessStatus {
            runtime: search.state(BINDING_BUILT).unwrap_or(VendorRuntime::Found),
            binding_built: BINDING_BUILT,
            sdk_version: if BINDING_BUILT { "3.0.2" } else { "none" },
            runtime_version: None,
            library: search.found,
            code: None,
            instance_extensions: Vec::new(),
            device_extensions: Vec::new(),
            temp_heaps: None,
        }
    }))
}
/// Latest XeSS discovery/init result. Reading status never loads a DLL.
#[must_use]
pub fn xess_status() -> XessStatus {
    status_with(|s| s.clone())
}

#[cfg(feature = "xess")]
fn runtime() -> Result<regia_xess::Runtime, regia_xess::Error> {
    if !BINDING_BUILT {
        return Err(regia_xess::Error::NotLinked);
    }
    let status = xess_status();
    let path = status.library.ok_or(regia_xess::Error::RuntimeNotFound)?;
    regia_xess::Runtime::load(&path)
}
/// Whether XeSS should take part in Vulkan device creation.
#[must_use]
pub fn wanted() -> bool {
    if !BINDING_BUILT {
        return false;
    }
    if regia_core::brand::env_var("REGIA_XESS").is_ok_and(|v| matches!(v.trim(), "0" | "off" | "false")) {
        status_with(|s| {
            s.code = Some("disabled_by_env");
            s.runtime = VendorRuntime::ReportsUnsupported;
        });
        return false;
    }
    xess_status().library.is_some()
}

#[cfg(any(feature = "xess", test))]
fn runtime_of(code: &str) -> VendorRuntime {
    match code {
        "not_linked" => VendorRuntime::NotBundled,
        "runtime_not_found" => VendorRuntime::NotFound,
        "needs_newer_driver" => VendorRuntime::NeedsNewerDriver,
        "unsupported_device" | "unsupported" | "not_vulkan" | "device_not_prepared" => {
            VendorRuntime::ReportsUnsupported
        }
        _ => VendorRuntime::InitFailed,
    }
}
#[cfg(feature = "xess")]
fn record_error(error: &regia_xess::Error) {
    status_with(|s| {
        s.runtime = runtime_of(error.code());
        s.code = Some(error.code());
    });
    tracing::warn!(%error, "XeSS unavailable; using the built-in temporal upscaler");
}
#[cfg(feature = "xess")]
pub(crate) fn hook_instance(desc: &wgpu::InstanceDescriptor) -> Option<wgpu::Instance> {
    if !wanted() || !desc.backends.contains(wgpu::Backends::VULKAN) {
        return None;
    }
    match runtime().and_then(|r| {
        let v = r.version();
        r.create_instance(desc).map(|pair| (pair, v))
    }) {
        Ok(((instance, extensions), v)) => {
            status_with(|s| {
                s.instance_extensions = extensions;
                s.runtime_version = Some([v.major, v.minor, v.patch]);
            });
            Some(instance)
        }
        Err(error) => {
            record_error(&error);
            None
        }
    }
}
#[cfg(feature = "xess")]
pub(crate) fn hook_device(
    adapter: &wgpu::Adapter,
    desc: &wgpu::DeviceDescriptor<'_>,
) -> Option<(wgpu::Device, wgpu::Queue)> {
    if !wanted() || adapter.get_info().backend != wgpu::Backend::Vulkan {
        return None;
    }
    let result = runtime().and_then(|r| {
        let (device, queue, extensions) = r.request_device(adapter, desc)?;
        // Probe the actual device, not merely a found DLL. No feature pipelines are built here.
        let _context = r.context(&device)?;
        Ok((device, queue, extensions))
    });
    match result {
        Ok((device, queue, extensions)) => {
            status_with(|s| {
                s.device_extensions = extensions;
                s.runtime = VendorRuntime::Ready;
                s.code = None;
            });
            Some((device, queue))
        }
        Err(error) => {
            record_error(&error);
            None
        }
    }
}
/// Whether this device was created with XeSS's full Vulkan requirements.
#[must_use]
pub fn device_prepared(device: &wgpu::Device) -> bool {
    #[cfg(feature = "xess")]
    {
        regia_xess::device_prepared(device)
    }
    #[cfg(not(feature = "xess"))]
    {
        let _ = device;
        false
    }
}
fn unavailable(runtime: VendorRuntime) -> UpscaleError {
    UpscaleError::RuntimeUnavailable {
        reason: runtime.blocker().unwrap_or(reason::SDK_NOT_BUNDLED),
    }
}
#[cfg(feature = "xess")]
fn sdk_error(error: regia_xess::Error) -> UpscaleError {
    record_error(&error);
    UpscaleError::Backend(format!("xess {}: {error}", error.code()))
}
#[cfg(feature = "xess")]
const fn quality(value: UpscaleQuality) -> regia_xess::Quality {
    match value {
        UpscaleQuality::Native => regia_xess::Quality::Native,
        UpscaleQuality::Quality => regia_xess::Quality::Quality,
        UpscaleQuality::Balanced => regia_xess::Quality::Balanced,
        UpscaleQuality::Performance => regia_xess::Quality::Performance,
        UpscaleQuality::UltraPerformance => regia_xess::Quality::UltraPerformance,
    }
}
#[cfg(feature = "xess")]
struct Live {
    runtime: regia_xess::Runtime,
    probe: regia_xess::Context,
    device: wgpu::Device,
    features: Vec<(FeatureHandle, regia_xess::Context)>,
    leases: Vec<regia_xess::Lease>,
    next: u64,
}
/// XeSS implementation of the renderer's vendor API, or an unavailable stub.
pub struct Xess {
    state: VendorRuntime,
    #[cfg(feature = "xess")]
    live: Option<Live>,
}
impl Xess {
    /// Attach to a device prepared by the XeSS hook. Failure is reported as a runtime state.
    #[must_use]
    pub fn for_device(device: &wgpu::Device) -> Self {
        #[cfg(feature = "xess")]
        {
            if !wanted() && xess_status().code == Some("disabled_by_env") {
                return Self {
                    state: VendorRuntime::ReportsUnsupported,
                    live: None,
                };
            }
            let result = runtime().and_then(|runtime| {
                let probe = runtime.context(device)?;
                Ok(Live {
                    runtime,
                    probe,
                    device: device.clone(),
                    features: Vec::new(),
                    leases: Vec::new(),
                    next: 1,
                })
            });
            match result {
                Ok(live) => {
                    status_with(|s| {
                        s.runtime = VendorRuntime::Ready;
                        s.code = None;
                    });
                    Self {
                        state: VendorRuntime::Ready,
                        live: Some(live),
                    }
                }
                Err(error) => {
                    record_error(&error);
                    Self {
                        state: runtime_of(error.code()),
                        live: None,
                    }
                }
            }
        }
        #[cfg(not(feature = "xess"))]
        {
            let _ = device;
            Self {
                state: VendorRuntime::NotBundled,
            }
        }
    }
}
impl VendorRuntimeApi for Xess {
    fn contract(&self) -> crate::upscaler::plugin::InputContract {
        crate::upscaler::plugin::InputContract {
            reactive: crate::upscaler::plugin::InputNeed::Unused,
            transparency: crate::upscaler::plugin::InputNeed::Unused,
            ..crate::upscaler::plugin::InputContract::VENDOR
        }
    }

    fn vendor(&self) -> VendorUpscaler {
        VendorUpscaler::Xess
    }
    fn runtime(&self) -> VendorRuntime {
        self.state
    }
    fn vulkan_extensions(&self) -> Result<VulkanExtensionRequest, UpscaleError> {
        if self.state != VendorRuntime::Ready {
            return Err(unavailable(self.state));
        }
        let status = xess_status();
        Ok(VulkanExtensionRequest {
            instance: status.instance_extensions,
            device: status.device_extensions,
        })
    }
    fn optimal_settings(
        &self,
        display: (u32, u32),
        mode: UpscaleQuality,
    ) -> Result<OptimalSettings, UpscaleError> {
        #[cfg(feature = "xess")]
        if let Some(live) = &self.live {
            let s = live
                .probe
                .optimal_settings([display.0, display.1], quality(mode))
                .map_err(sdk_error)?;
            return Ok(OptimalSettings {
                render_size: (s.render[0], s.render[1]),
                render_min: (s.min[0], s.min[1]),
                render_max: (s.max[0], s.max[1]),
                sharpness: 0.0,
            });
        }
        let _ = (display, mode);
        Err(unavailable(self.state))
    }
    fn release_feature(&mut self, handle: FeatureHandle) {
        #[cfg(feature = "xess")]
        if let Some(live) = &mut self.live {
            live.features.retain(|(h, _)| *h != handle);
        }
        let _ = handle;
    }
}
impl VendorEvaluate<GpuTarget> for Xess {
    fn create_feature(
        &mut self,
        ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        desc: &FeatureDesc,
        settings: &OptimalSettings,
    ) -> Result<FeatureHandle, UpscaleError> {
        #[cfg(feature = "xess")]
        if let Some(live) = &mut self.live {
            if *ctx.device != live.device {
                return Err(UpscaleError::Backend(
                    "xess invalid_input: device mismatch".into(),
                ));
            }
            let mut context = live.runtime.context(ctx.device).map_err(sdk_error)?;
            context
                .initialize(regia_xess::CreateDesc {
                    display: [desc.display_size.0, desc.display_size.1],
                    quality: quality(desc.quality),
                    depth_inverted: desc.flags.depth_inverted,
                    hdr: desc.flags.hdr,
                    motion_jittered: desc.flags.motion_vectors_jittered,
                })
                .map_err(sdk_error)?;
            let heaps = context
                .temp_heap_zeroing()
                .map_or(["sdk_allocated"; 2], |methods| {
                    methods.map(regia_xess::HeapZeroing::code)
                });
            status_with(|s| s.temp_heaps = Some(heaps));
            let handle = FeatureHandle(live.next);
            live.next += 1;
            live.features.push((handle, context));
            return Ok(handle);
        }
        let _ = (ctx, desc, settings);
        Err(unavailable(self.state))
    }
    fn evaluate(
        &mut self,
        handle: FeatureHandle,
        ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        frame: &UpscaleFrame<'_, wgpu::TextureView>,
        eval: &VendorEvalParams,
    ) -> Result<(), UpscaleError> {
        #[cfg(feature = "xess")]
        if let Some(live) = &mut self.live {
            let _span = tracing::info_span!("xess_vendor_evaluate").entered();
            // Frame N was submitted before frame N+1 enters the slot.
            live.leases.clear();
            let depth = frame.depth.ok_or(UpscaleError::MissingInput { input: "depth" })?;
            let motion = frame
                .motion
                .ok_or(UpscaleError::MissingInput { input: "motion" })?;
            let (_, context) = live
                .features
                .iter_mut()
                .find(|(h, _)| *h == handle)
                .ok_or_else(|| UpscaleError::Backend("xess feature_not_found".into()))?;
            let inputs = regia_xess::EvalInputs {
                color: frame.color,
                depth,
                motion,
                output: ctx.output,
                jitter: eval.jitter,
                motion_scale: eval.motion_scale,
                exposure: eval.exposure,
                render_size: [eval.render_size.0, eval.render_size.1],
                reset: eval.reset,
            };
            let (commands, lease) = context
                .evaluate(ctx.device, ctx.encoder, &inputs)
                .map_err(sdk_error)?
                .into_parts();
            live.leases.push(lease);
            let fresh = ctx
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("frame after xess"),
                });
            let before = std::mem::replace(ctx.encoder, fresh);
            ctx.finished.push(before.finish());
            ctx.finished.push(commands);
            status_with(|s| {
                s.runtime = VendorRuntime::Ready;
                s.code = None;
            });
            return Ok(());
        }
        let _ = (handle, ctx, frame, eval);
        Err(unavailable(self.state))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_error_states_are_actionable() {
        assert_eq!(runtime_of("not_linked"), VendorRuntime::NotBundled);
        assert_eq!(runtime_of("runtime_not_found"), VendorRuntime::NotFound);
        assert_eq!(
            runtime_of("unsupported_device"),
            VendorRuntime::ReportsUnsupported
        );
        assert_eq!(runtime_of("needs_newer_driver"), VendorRuntime::NeedsNewerDriver);
        assert_eq!(runtime_of("backend_error"), VendorRuntime::InitFailed);
    }
}
