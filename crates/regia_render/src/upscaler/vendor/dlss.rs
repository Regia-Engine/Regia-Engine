//! DLSS Super Resolution through NGX behind the vendor seam (ADR-2002).
//!
//! - [`DlssNgx`]: the binding. With the `dlss` cargo feature and an NGX-linked `regia_dlss`
//!   (`DLSS_SDK` set at build time) it initialises NGX on the render device and implements
//!   [`VendorRuntimeApi`] and [`VendorEvaluate`] for [`GpuTarget`]; otherwise it reports
//!   [`VendorRuntime::NotBundled`] and refuses every call, like [`super::DlssSdk`].
//! - [`dlss_status`]: the process-wide state the agent protocol reports
//!   (`render.get_upscalers` → `runtime`): built or not, the library found, the extensions
//!   device creation added, NGX's last answer.
//! - The device-creation hook (`device.rs` calls `hook_instance` and `hook_device`): when the
//!   binding is built, `REGIA_DLSS` is not `0` and the runtime library is found, the instance and
//!   device are created with the Vulkan extensions NGX asks for. Any failure falls back to the
//!   normal creation path with a warning.
//! - `FrameVendor`: the frame's vendor slot. It upscales a single-sample TAA frame through a
//!   [`VendorBackend`], and on any refusal or failure the frame runs its built-in temporal
//!   upscaler instead, with the reason kept for `render.get_vendor_upscaler`.
//!
//! NGX records its evaluate on a command buffer of its own (wgpu does not allow raw and wgpu
//! recording in one encoder), so the binding ends the frame's encoder at the upscale: the
//! finished part and the DLSS buffer go to [`GpuUpscaleContext::finished`], which the frame
//! submits in order before the rest of the frame.

use std::path::PathBuf;
use std::sync::Mutex;

use super::{
    FeatureDesc, FeatureHandle, OptimalSettings, RuntimeLibrary, VendorBackend, VendorEvalParams,
    VendorEvaluate, VendorRuntimeApi, VulkanExtensionRequest,
};
use crate::upscaler::plugin::{
    GpuTarget, GpuUpscaleContext, InputContract, UpscaleError, UpscaleFrame, UpscaleTarget, Upscaler,
    VendorRuntime,
};
use crate::upscaler::{UpscaleQuality, VendorUpscaler, reason};

/// Environment variable: `0` / `off` keeps DLSS out of device creation even when the binding
/// is built and the runtime is found.
pub const DLSS_ENV: &str = "REGIA_DLSS";

/// The binding is in this build: the `dlss` feature with an NGX-linked `regia_dlss`.
pub const BINDING_BUILT: bool = binding_built();

const fn binding_built() -> bool {
    #[cfg(feature = "dlss")]
    {
        regia_dlss::LINKED
    }
    #[cfg(not(feature = "dlss"))]
    {
        false
    }
}

/// The DLSS SDK version string from `regia_dlss::SDK_VERSION` when the `dlss` feature is on,
/// otherwise `none`.
///
/// A linked Linux build is the `.so` suffix (a dotted version). A Windows build whose runtime
/// file is `nvngx_dlss.dll` is `windows`. The check stub is `check-stub`. A stub build, a missing
/// SDK, and Android or macOS (no NGX library) are `none`. A linked build that finds neither
/// redistributable name is `unknown`.
#[must_use]
pub fn sdk_version() -> &'static str {
    #[cfg(feature = "dlss")]
    {
        regia_dlss::SDK_VERSION
    }
    #[cfg(not(feature = "dlss"))]
    {
        "none"
    }
}

/// What this process knows about DLSS, for the agent protocol and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DlssStatus {
    /// The runtime state: [`VendorRuntime::Found`] until NGX answered for a device, then what
    /// the last answer means (ready, or why not, as the binding's code maps to a state).
    pub runtime: VendorRuntime,
    /// The binding is built ([`BINDING_BUILT`]).
    pub binding_built: bool,
    /// The SDK version linked ([`sdk_version`]).
    pub sdk_version: &'static str,
    /// The runtime library the search found.
    pub library: Option<PathBuf>,
    /// The last code from NGX or the binding (`unavailable`, `needs_newer_driver`,
    /// `not_vulkan`, an NGX failure name, `disabled_by_env`, ...), if any.
    pub code: Option<&'static str>,
    /// Vulkan instance extensions device creation added for DLSS.
    pub instance_extensions: Vec<String>,
    /// Vulkan device extensions device creation added for DLSS.
    pub device_extensions: Vec<String>,
}

static STATUS: Mutex<Option<DlssStatus>> = Mutex::new(None);

fn initial_status() -> DlssStatus {
    let search = if BINDING_BUILT {
        RuntimeLibrary::current(VendorUpscaler::Dlss).locate_for_this_process()
    } else {
        super::RuntimeSearch::default()
    };
    let runtime = search.state(BINDING_BUILT).unwrap_or(VendorRuntime::Found);
    DlssStatus {
        runtime,
        binding_built: BINDING_BUILT,
        sdk_version: sdk_version(),
        library: search.found,
        code: None,
        instance_extensions: Vec::new(),
        device_extensions: Vec::new(),
    }
}

fn with_status<T>(body: impl FnOnce(&mut DlssStatus) -> T) -> T {
    let mut guard = STATUS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let status = guard.get_or_insert_with(initial_status);
    body(status)
}

/// The process-wide DLSS state (computed on first use: the binding and the library search).
#[must_use]
pub fn dlss_status() -> DlssStatus {
    with_status(|status| status.clone())
}

/// Whether `device` enables every Vulkan extension NGX asks for on that device.
///
/// `Some(true)` includes an empty requirement list: nothing was missing. `Some(false)` is a
/// device whose requirements are not enabled, or a check that failed. `None` when this build
/// has no DLSS binding, so the caller cannot ask NGX and must use the recorded hook list.
#[must_use]
pub fn ngx_extensions_on_device(device: &wgpu::Device) -> Option<bool> {
    #[cfg(not(feature = "dlss"))]
    {
        let _ = device;
        None
    }
    #[cfg(feature = "dlss")]
    {
        Some(regia_dlss::device_has_ngx_extensions(device, &ngx_config()).unwrap_or(false))
    }
}

/// The `dlss_vulkan_extensions` flag for one device.
///
/// `per_device` is [`ngx_extensions_on_device`]. `Some(true)` wins even when the last hook
/// recorded no names: that list is empty both when NGX asked for nothing and when an extension
/// was missing. `None` (no DLSS binding) falls back to `recorded_nonempty`.
#[must_use]
pub fn dlss_vulkan_extensions_flag(per_device: Option<bool>, recorded_nonempty: bool) -> bool {
    per_device.unwrap_or(recorded_nonempty)
}

/// DLSS may take part in device creation: the binding is built, `REGIA_DLSS` does not turn it
/// off and the runtime library was found.
#[must_use]
pub fn wanted() -> bool {
    if !BINDING_BUILT {
        return false;
    }
    let off = regia_core::brand::env_var(DLSS_ENV).is_ok_and(|v| matches!(v.trim(), "0" | "off" | "false"));
    if off {
        with_status(|status| status.code = Some("disabled_by_env"));
        return false;
    }
    with_status(|status| status.library.is_some())
}

#[cfg(feature = "dlss")]
fn ngx_config() -> regia_dlss::NgxConfig {
    let dirs = with_status(|status| {
        status
            .library
            .as_ref()
            .and_then(|lib| lib.parent().map(PathBuf::from))
    });
    regia_dlss::NgxConfig::new(dirs.into_iter().collect())
}

/// The runtime state a `regia_dlss` error code means (ADR-2679): the build has no SDK, the driver
/// is too old, NGX refuses this GPU (`unavailable`, or `feature_not_supported` already at device
/// creation: a GTX 1080 with driver 580.173.02 answers that, ADR-4937), or anything else went wrong
/// while starting it.
#[cfg(any(feature = "dlss", test))]
fn runtime_of_code(code: &str) -> VendorRuntime {
    match code {
        "not_linked" => VendorRuntime::NotBundled,
        "needs_newer_driver" => VendorRuntime::NeedsNewerDriver,
        "unavailable" | "feature_not_supported" | "not_vulkan" | "not_implemented" => {
            VendorRuntime::ReportsUnsupported
        }
        _ => VendorRuntime::InitFailed,
    }
}

#[cfg(all(feature = "dlss", test))]
fn runtime_of(error: &regia_dlss::DlssError) -> VendorRuntime {
    runtime_of_code(error.code())
}

/// Records a failed NGX step in `status`: its code and the runtime state that code means
/// (ADR-4937). Every failure path writes both, so the process-wide status the agent reads names
/// the same state the device's binding fell back on. A GTX 1080 is refused at the device hook
/// (`feature_not_supported`); before this the status kept `found` there.
#[cfg(any(feature = "dlss", test))]
fn record_failure(status: &mut DlssStatus, code: &'static str) -> VendorRuntime {
    let runtime = runtime_of_code(code);
    status.runtime = runtime;
    status.code = Some(code);
    runtime
}

/// The instance for `descriptor` with DLSS's instance extensions, or `None` (DLSS not wanted, or
/// the hook failed: the caller creates the instance as usual).
#[cfg(feature = "dlss")]
pub(crate) fn hook_instance(descriptor: &wgpu::InstanceDescriptor) -> Option<wgpu::Instance> {
    if !wanted() || !descriptor.backends.contains(wgpu::Backends::VULKAN) {
        return None;
    }
    match regia_dlss::create_instance(descriptor, &ngx_config()) {
        Ok((instance, added)) => {
            tracing::info!(target: "regia_render", ?added, "DLSS: Vulkan instance created with NGX's extensions");
            with_status(|status| status.instance_extensions = added);
            Some(instance)
        }
        Err(error) => {
            tracing::warn!(target: "regia_render", %error, "DLSS: instance hook failed; creating the instance without it");
            with_status(|status| status.code = Some(error.code()));
            None
        }
    }
}

/// A device on `adapter` with DLSS's device extensions, or `None` (DLSS not wanted, not a
/// Vulkan NVIDIA adapter, or the hook failed: the caller requests the device as usual).
#[cfg(feature = "dlss")]
pub(crate) fn hook_device(
    adapter: &wgpu::Adapter,
    descriptor: &wgpu::DeviceDescriptor<'_>,
) -> Option<(wgpu::Device, wgpu::Queue)> {
    let info = adapter.get_info();
    if !wanted() || info.backend != wgpu::Backend::Vulkan || info.vendor != 0x10DE {
        return None;
    }
    match regia_dlss::request_device(adapter, descriptor, &ngx_config()) {
        Ok((device, queue, added)) => {
            tracing::info!(target: "regia_render", ?added, "DLSS: device created with NGX's extensions");
            with_status(|status| status.device_extensions = added);
            Some((device, queue))
        }
        Err(error) => {
            tracing::warn!(target: "regia_render", %error, "DLSS: device hook failed; requesting the device without it");
            with_status(|status| record_failure(status, error.code()));
            None
        }
    }
}

/// [`UpscaleQuality`] → DLSS mode (Native is DLAA).
#[cfg(feature = "dlss")]
const fn perf_quality(quality: UpscaleQuality) -> regia_dlss::PerfQuality {
    match quality {
        UpscaleQuality::Native => regia_dlss::PerfQuality::Dlaa,
        UpscaleQuality::Quality => regia_dlss::PerfQuality::Quality,
        UpscaleQuality::Balanced => regia_dlss::PerfQuality::Balanced,
        UpscaleQuality::Performance => regia_dlss::PerfQuality::Performance,
        UpscaleQuality::UltraPerformance => regia_dlss::PerfQuality::UltraPerformance,
    }
}

#[cfg(feature = "dlss")]
const fn create_flags(flags: super::FeatureFlags) -> regia_dlss::CreateFlags {
    regia_dlss::CreateFlags {
        hdr: flags.hdr,
        depth_inverted: flags.depth_inverted,
        motion_low_res: flags.motion_vectors_render_size,
        motion_jittered: flags.motion_vectors_jittered,
        auto_exposure: flags.auto_exposure,
    }
}

fn not_ready(runtime: VendorRuntime) -> UpscaleError {
    UpscaleError::RuntimeUnavailable {
        reason: runtime.blocker().unwrap_or(reason::SDK_NOT_BUNDLED),
    }
}

#[cfg(feature = "dlss")]
fn ngx_error(error: &regia_dlss::DlssError) -> UpscaleError {
    tracing::warn!(target: "regia_render", %error, "DLSS call failed");
    UpscaleError::Backend(format!("dlss {}: {error}", error.code()))
}

/// NGX on one device, with the features it created. `regia_dlss` shares the NGX instance
/// between every binding on that device (one init, one shutdown per device; audit 11-03, dlss
/// M1); it is `None` only while the binding drops.
#[cfg(feature = "dlss")]
struct Live {
    ngx: Option<regia_dlss::Ngx>,
    features: Vec<(FeatureHandle, regia_dlss::SuperResolution)>,
    next: u64,
    exposure: Option<(wgpu::Texture, wgpu::TextureView, f32)>,
    /// Leases of the DLSS buffers handed to the frame since the previous `evaluate`. The frame
    /// submits them at its end, before the next frame evaluates, so a feature released in
    /// between stays alive until its last buffer was submitted (audit 11-03, dlss H1).
    in_flight: Vec<regia_dlss::FeatureLease>,
}

/// The DLSS Super Resolution binding through NGX (ADR-2002). See the module docs.
pub struct DlssNgx {
    runtime: VendorRuntime,
    #[cfg(feature = "dlss")]
    live: Option<Box<Live>>,
}

impl std::fmt::Debug for DlssNgx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DlssNgx")
            .field("runtime", &self.runtime)
            .finish_non_exhaustive()
    }
}

impl DlssNgx {
    /// A binding with no device: [`VendorRuntime::NotBundled`] without the binding,
    /// [`VendorRuntime::NotFound`] without the runtime library, else [`VendorRuntime::Found`].
    /// Each device decides for itself (another device of the process may be ready).
    #[must_use]
    pub fn unattached() -> Self {
        let status = dlss_status();
        let runtime = if !status.binding_built {
            VendorRuntime::NotBundled
        } else if status.library.is_none() {
            VendorRuntime::NotFound
        } else {
            VendorRuntime::Found
        };
        Self {
            runtime,
            #[cfg(feature = "dlss")]
            live: None,
        }
    }

    /// Initialises NGX on `device`, which must come from the device-creation hook (it needs
    /// NGX's extensions). Never fails: the state and [`dlss_status`] say why it is not ready.
    #[must_use]
    pub fn for_device(device: &wgpu::Device) -> Self {
        #[cfg(not(feature = "dlss"))]
        {
            let _ = device;
            Self::unattached()
        }
        #[cfg(feature = "dlss")]
        {
            let mut binding = Self::unattached();
            if binding.runtime != VendorRuntime::Found {
                return binding;
            }
            // This device must enable every extension NGX asks for on its GPU (audit wave 6,
            // upscalers M2: a second device, or one made after a failed hook, went to NGX without
            // its extensions). Asked from NGX for this device, not read from the process-wide
            // status, which holds the lists of whichever device the hooks made last and where an
            // empty list was both "none wanted" and "unsupported" (audit 11-03, dlss M2, L1).
            match regia_dlss::device_has_ngx_extensions(device, &ngx_config()) {
                Ok(true) => {}
                Ok(false) => {
                    binding.runtime = VendorRuntime::ReportsUnsupported;
                    with_status(|s| {
                        s.runtime = VendorRuntime::ReportsUnsupported;
                        s.code = Some("device_without_ngx_extensions");
                    });
                    return binding;
                }
                Err(error) => {
                    tracing::warn!(target: "regia_render", %error, "DLSS: NGX's extension check failed; the built-in upscaler runs");
                    binding.runtime = with_status(|s| record_failure(s, error.code()));
                    return binding;
                }
            }
            match regia_dlss::Ngx::new(&ngx_config(), device) {
                Ok(ngx) => {
                    tracing::info!(target: "regia_render", sdk = regia_dlss::SDK_VERSION, "DLSS: NGX ready");
                    binding.runtime = VendorRuntime::Ready;
                    binding.live = Some(Box::new(Live {
                        ngx: Some(ngx),
                        features: Vec::new(),
                        next: 1,
                        exposure: None,
                        in_flight: Vec::new(),
                    }));
                    with_status(|s| {
                        s.runtime = VendorRuntime::Ready;
                        s.code = None;
                    });
                }
                Err(error) => {
                    tracing::warn!(target: "regia_render", %error, "DLSS: NGX did not initialise; the built-in upscaler runs");
                    binding.runtime = with_status(|s| record_failure(s, error.code()));
                }
            }
            binding
        }
    }
}

/// A binding drops its features, then its NGX handle, with no process-wide lock held: each
/// feature waits for the GPU before its release, and `regia_dlss` makes a re-init on the same
/// device wait for NGX's shutdown while other devices go on (audit 11-03, dlss M3: the seam held
/// one lock across every teardown's GPU waits and every device's native init).
#[cfg(feature = "dlss")]
impl Drop for Live {
    fn drop(&mut self) {
        self.in_flight.clear();
        self.features.clear();
        self.ngx = None;
    }
}

impl VendorRuntimeApi for DlssNgx {
    fn vendor(&self) -> VendorUpscaler {
        VendorUpscaler::Dlss
    }

    fn runtime(&self) -> VendorRuntime {
        self.runtime
    }

    fn vulkan_extensions(&self) -> Result<VulkanExtensionRequest, UpscaleError> {
        if self.runtime != VendorRuntime::Ready {
            return Err(not_ready(self.runtime));
        }
        let status = dlss_status();
        Ok(VulkanExtensionRequest {
            instance: status.instance_extensions,
            device: status.device_extensions,
        })
    }

    fn optimal_settings(
        &self,
        display: (u32, u32),
        quality: UpscaleQuality,
    ) -> Result<OptimalSettings, UpscaleError> {
        #[cfg(feature = "dlss")]
        if let Some(live) = self.live.as_ref()
            && let Some(ngx) = live.ngx.as_ref()
        {
            let settings = ngx
                .optimal_settings([display.0, display.1], perf_quality(quality))
                .map_err(|e| ngx_error(&e))?;
            return Ok(OptimalSettings {
                render_size: (settings.render[0], settings.render[1]),
                render_min: (settings.min[0], settings.min[1]),
                render_max: (settings.max[0], settings.max[1]),
                sharpness: settings.sharpness,
            });
        }
        let _ = (display, quality);
        Err(not_ready(self.runtime))
    }

    fn release_feature(&mut self, feature: FeatureHandle) {
        #[cfg(feature = "dlss")]
        if let Some(live) = self.live.as_mut() {
            // Dropping the feature waits for the device and releases it in NGX.
            live.features.retain(|(handle, _)| *handle != feature);
        }
        let _ = feature;
    }
}

impl VendorEvaluate<GpuTarget> for DlssNgx {
    fn create_feature(
        &mut self,
        ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        desc: &FeatureDesc,
        settings: &OptimalSettings,
    ) -> Result<FeatureHandle, UpscaleError> {
        #[cfg(feature = "dlss")]
        if let Some(live) = self.live.as_mut()
            && let Some(ngx) = live.ngx.as_ref()
        {
            let feature = regia_dlss::SuperResolution::new(
                ngx,
                ctx.device,
                ctx.queue,
                [desc.display_size.0, desc.display_size.1],
                perf_quality(desc.quality),
                create_flags(desc.flags),
                [settings.render_max.0, settings.render_max.1],
            )
            .map_err(|e| ngx_error(&e))?;
            let handle = FeatureHandle(live.next);
            live.next += 1;
            live.features.push((handle, feature));
            return Ok(handle);
        }
        let _ = (ctx, desc, settings);
        Err(not_ready(self.runtime))
    }

    fn evaluate(
        &mut self,
        feature: FeatureHandle,
        ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        frame: &UpscaleFrame<'_, wgpu::TextureView>,
        eval: &VendorEvalParams,
    ) -> Result<(), UpscaleError> {
        #[cfg(feature = "dlss")]
        if let Some(live) = self.live.as_mut() {
            let _span = tracing::info_span!("dlss_ngx_evaluate").entered();
            let _crumb = crate::breadcrumbs::frame_pass("dlss_ngx_evaluate");
            // One evaluate per binding per frame (`VendorBackend` holds one live feature), and the
            // previous frame was submitted before this one records (`in_flight`).
            live.in_flight.clear();
            let (Some(depth), Some(motion)) = (frame.depth, frame.motion) else {
                return Err(UpscaleError::MissingInput {
                    input: if frame.depth.is_none() { "depth" } else { "motion" },
                });
            };
            // A 1×1 exposure multiplier, written only when it changes.
            let exposure = match live.exposure.take() {
                Some(existing) => existing,
                None => {
                    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("dlss exposure"),
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
                    });
                    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                    (texture, view, f32::NAN)
                }
            };
            let (texture, view, written) = exposure;
            if written.to_bits() != eval.exposure.to_bits() {
                ctx.queue.write_texture(
                    texture.as_image_copy(),
                    &eval.exposure.to_le_bytes(),
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(4),
                        rows_per_image: Some(1),
                    },
                    texture.size(),
                );
            }
            let Some((_, sr)) = live.features.iter_mut().find(|(handle, _)| *handle == feature) else {
                live.exposure = Some((texture, view, eval.exposure));
                return Err(UpscaleError::Backend(
                    "dlss feature_not_found: no live feature for the handle".into(),
                ));
            };
            let inputs = regia_dlss::EvalInputs {
                color: frame.color,
                depth,
                motion,
                exposure: Some(&view),
                reactive: frame.reactive,
                transparency: frame.transparency,
                output: ctx.output,
                jitter: eval.jitter,
                motion_scale: eval.motion_scale,
                render_size: [eval.render_size.0, eval.render_size.1],
                reset: eval.reset,
                frame_time_ms: eval.frame_time_ms,
            };
            let result = sr.evaluate(ctx.device, ctx.encoder, &inputs);
            live.exposure = Some((texture, view, eval.exposure));
            let (dlss, lease) = result.map_err(|e| ngx_error(&e))?.into_parts();
            live.in_flight.push(lease);
            // NGX's buffer runs between the frame's work so far and the rest of the frame.
            let fresh = ctx
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("frame after dlss"),
                });
            let before = std::mem::replace(&mut *ctx.encoder, fresh);
            ctx.finished.push(before.finish());
            ctx.finished.push(dlss);
            return Ok(());
        }
        let _ = (feature, ctx, frame, eval);
        Err(not_ready(self.runtime))
    }
}

/// DLSS through NGX behind the boundary.
pub type DlssNgxUpscaler = VendorBackend<DlssNgx>;

/// What the frame's vendor slot did last, for `render.get_vendor_upscaler`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VendorFrameReport {
    /// The vendor the host selected, if any.
    pub selected: Option<VendorUpscaler>,
    /// The last frame ran the vendor upscaler (otherwise the built-in one ran).
    pub active: bool,
    /// Why the last frame fell back to the built-in upscaler (an [`UpscaleError::code`]).
    pub fallback: Option<&'static str>,
    /// Frames the vendor upscaled.
    pub frames: u64,
    /// Frames that fell back while a vendor was selected.
    pub fallbacks: u64,
    /// The runtime state of the selected vendor's binding.
    pub runtime: Option<VendorRuntime>,
    /// Render size the live feature accepts, if one exists.
    pub render_range: Option<((u32, u32), (u32, u32))>,
    /// Render size the runtime prefers at the frame's mode, once it can answer (ADR-2679).
    pub render_preferred: Option<(u32, u32)>,
}

/// A vendor binding behind the frame's slot: anything that implements the seam on the GPU target
/// (ADR-2679). The slot does not know which vendor it holds, so XeSS plugs in where DLSS does.
pub trait GpuBinding: VendorEvaluate<GpuTarget> + Send + Sync {}

impl<S: VendorEvaluate<GpuTarget> + Send + Sync> GpuBinding for S {}

impl VendorRuntimeApi for Box<dyn GpuBinding> {
    fn vendor(&self) -> VendorUpscaler {
        (**self).vendor()
    }

    fn runtime(&self) -> VendorRuntime {
        (**self).runtime()
    }

    fn contract(&self) -> InputContract {
        (**self).contract()
    }

    fn vulkan_extensions(&self) -> Result<VulkanExtensionRequest, UpscaleError> {
        (**self).vulkan_extensions()
    }

    fn optimal_settings(
        &self,
        display: (u32, u32),
        quality: UpscaleQuality,
    ) -> Result<OptimalSettings, UpscaleError> {
        (**self).optimal_settings(display, quality)
    }

    fn release_feature(&mut self, feature: FeatureHandle) {
        (**self).release_feature(feature);
    }
}

impl VendorEvaluate<GpuTarget> for Box<dyn GpuBinding> {
    fn create_feature(
        &mut self,
        ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        desc: &FeatureDesc,
        settings: &OptimalSettings,
    ) -> Result<FeatureHandle, UpscaleError> {
        (**self).create_feature(ctx, desc, settings)
    }

    fn evaluate(
        &mut self,
        feature: FeatureHandle,
        ctx: &mut <GpuTarget as UpscaleTarget>::Context<'_>,
        frame: &UpscaleFrame<'_, wgpu::TextureView>,
        eval: &VendorEvalParams,
    ) -> Result<(), UpscaleError> {
        (**self).evaluate(feature, ctx, frame, eval)
    }
}

/// The binding the frame's slot uses for `vendor`: a test's scripted binding if one is installed
/// ([`super::scripted::install_binding_factory`]), else this build's.
fn binding_for(vendor: VendorUpscaler, device: &wgpu::Device) -> Box<dyn GpuBinding> {
    if let Some(binding) = super::scripted::binding_override(vendor, device) {
        return binding;
    }
    match vendor {
        VendorUpscaler::Dlss => Box::new(DlssNgx::for_device(device)),
        VendorUpscaler::Xess => Box::new(super::xess::Xess::for_device(device)),
    }
}

/// The mode a frame's feature runs at. The mode the frame asked for, unless the live feature for
/// this display already accepts the render size: a dynamic-resolution step across the middle of
/// two modes then keeps the feature instead of recreating it and dropping its history (ADR-2679).
fn feature_mode(
    asked: UpscaleQuality,
    live: Option<(FeatureDesc, OptimalSettings)>,
    display: (u32, u32),
    render: (u32, u32),
) -> UpscaleQuality {
    match live {
        Some((desc, settings)) if desc.display_size == display && settings.accepts(render) => desc.quality,
        _ => asked,
    }
}

/// The frame's vendor upscaler slot: what the host selected and, once the frame has a device,
/// the binding.
#[derive(Default)]
pub(crate) struct FrameVendor {
    selected: Option<VendorUpscaler>,
    backend: Option<VendorBackend<Box<dyn GpuBinding>>>,
    output: Option<(wgpu::Texture, wgpu::TextureView)>,
    report: VendorFrameReport,
    /// The mode [`Self::render_extent`] chose for this frame, used by its upscale.
    frame_mode: Option<UpscaleQuality>,
}

impl FrameVendor {
    /// Selects a vendor (or none); a change drops the binding, which the next frame recreates.
    pub(crate) fn select(&mut self, vendor: Option<VendorUpscaler>) {
        if self.selected != vendor {
            self.selected = vendor;
            self.backend = None;
            self.frame_mode = None;
            self.report = VendorFrameReport {
                selected: vendor,
                ..VendorFrameReport::default()
            };
        }
    }

    pub(crate) fn selected(&self) -> Option<VendorUpscaler> {
        self.selected
    }

    pub(crate) fn report(&self) -> VendorFrameReport {
        self.report.clone()
    }

    fn output_for(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        size: (u32, u32),
    ) -> Result<wgpu::TextureView, UpscaleError> {
        if let Some((texture, view)) = self.output.as_ref()
            && texture.width() == size.0
            && texture.height() == size.1
        {
            return Ok(view.clone());
        }
        let (zero_bytes, row_bytes) = output_zero_fill(size, device.limits().max_texture_dimension_2d)?;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vendor upscale output"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // wgpu zero-fills a texture before its first read unless wgpu itself wrote it, and NGX's
        // writes are invisible to it: one write through wgpu marks it initialised.
        let bytes = vec![0u8; zero_bytes];
        queue.write_texture(
            texture.as_image_copy(),
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row_bytes),
                rows_per_image: Some(size.1),
            },
            texture.size(),
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.output = Some((texture, view.clone()));
        Ok(view)
    }

    /// The render size of a frame whose scale asks for `requested` (ADR-2679). A frame with no
    /// vendor selected, or that the vendor will not run (`vendor_frame` false: MSAA, no TAA),
    /// keeps `requested`, as does a binding that cannot run: those frames do not move. Otherwise
    /// the runtime's range decides ([`super::PreferredRange::frame_extent`]). The binding starts
    /// here so a frame's first image is already at the size it keeps.
    pub(crate) fn render_extent(
        &mut self,
        device: &wgpu::Device,
        requested: (u32, u32),
        display: (u32, u32),
        quality: &crate::FrameQuality,
        vendor_frame: bool,
    ) -> (u32, u32) {
        self.frame_mode = None;
        let Some(vendor) = self.selected.filter(|_| vendor_frame) else {
            return requested;
        };
        let mode = quality
            .upscale_quality
            .unwrap_or_else(|| UpscaleQuality::nearest(requested, display));
        self.backend
            .get_or_insert_with(|| VendorBackend::new(binding_for(vendor, device), mode));
        self.extent_with_backend(requested, display, quality)
    }

    /// [`Self::render_extent`] once the slot holds its binding.
    fn extent_with_backend(
        &mut self,
        requested: (u32, u32),
        display: (u32, u32),
        quality: &crate::FrameQuality,
    ) -> (u32, u32) {
        let Some(backend) = self.backend.as_ref() else {
            return requested;
        };
        let live = backend.live_feature().zip(backend.live_settings());
        let asked = quality
            .upscale_quality
            .unwrap_or_else(|| UpscaleQuality::nearest(requested, display));
        // A host's mode is the mode; a scale the live feature accepts keeps the feature's.
        let mode = match quality.upscale_quality {
            Some(mode) => mode,
            None => feature_mode(asked, live, display, requested),
        };
        let Some(range) = backend.preferred_range(display, mode) else {
            self.report.render_preferred = None;
            // What a live feature still accepts, else nothing: the last range is not live.
            self.report.render_range = backend.live_settings().map(|s| (s.render_min, s.render_max));
            return requested;
        };
        self.report.render_preferred = Some(range.preferred);
        self.report.render_range = Some((range.min, range.max));
        self.frame_mode = Some(mode);
        range.frame_extent(
            requested,
            u32::from(quality.render_scale_permille),
            quality.upscale_quality,
        )
    }

    /// Upscales the frame through the selected vendor, or returns why it could not (the frame
    /// then runs its built-in upscaler). `finished` receives the command buffers to submit, in
    /// order, before the rest of `encoder`.
    pub(crate) fn upscale(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        finished: &mut Vec<wgpu::CommandBuffer>,
        frame: &UpscaleFrame<'_, wgpu::TextureView>,
        quality: UpscaleQuality,
    ) -> Option<wgpu::TextureView> {
        let vendor = self.selected?;
        let _span = tracing::info_span!("vendor_frame_upscale", vendor = vendor.name()).entered();
        let frame_mode = self.frame_mode.take();
        let backend = self
            .backend
            .get_or_insert_with(|| VendorBackend::new(binding_for(vendor, device), quality));
        let live = backend.live_feature().zip(backend.live_settings());
        let mode = frame_mode.unwrap_or_else(|| {
            feature_mode(quality, live, frame.params.display_size, frame.params.render_size)
        });
        backend.set_quality(mode);
        let runtime = backend.runtime();
        self.report.runtime = Some(runtime);
        // A binding that cannot run gets no output texture.
        let result = match runtime.blocker() {
            Some(reason) => Err(UpscaleError::RuntimeUnavailable { reason }),
            None => Ok(()),
        };
        let result = result.and_then(|()| {
            let output = self.output_for(device, queue, frame.params.display_size)?;
            let backend = self.backend.as_mut().ok_or(UpscaleError::RuntimeUnavailable {
                reason: reason::SDK_NOT_BUNDLED,
            })?;
            let mut ctx = GpuUpscaleContext {
                device,
                queue,
                encoder,
                output: &output,
                finished,
            };
            Upscaler::<GpuTarget>::upscale(backend, &mut ctx, frame)?;
            self.report.render_range = backend.live_settings().map(|s| (s.render_min, s.render_max));
            Ok(output)
        });
        match result {
            Ok(output) => {
                self.report.active = true;
                self.report.fallback = None;
                self.report.frames += 1;
                Some(output)
            }
            Err(error) => {
                if self.report.fallback != Some(error.code()) {
                    tracing::info!(
                        target: "regia_render",
                        vendor = vendor.name(),
                        %error,
                        "vendor upscaler not used; the built-in upscaler runs"
                    );
                }
                self.report.active = false;
                self.report.fallback = Some(error.code());
                self.report.fallbacks += 1;
                None
            }
        }
    }
}

/// The zero-fill of a vendor output texture of `size` (`Rgba16Float`, 8 bytes a texel): its byte
/// count and row pitch, or a backend error, so the frame falls back to the built-in upscaler, for a
/// size the device cannot hold or whose fill does not fit the arithmetic (audit wave 5, upscalers
/// M1 and L3).
fn output_zero_fill(size: (u32, u32), max_dimension: u32) -> Result<(usize, u32), UpscaleError> {
    let refuse = || {
        UpscaleError::Backend(format!(
            "vendor output {}x{} is outside the device's {max_dimension} px texture limit",
            size.0, size.1
        ))
    };
    if size.0 == 0 || size.1 == 0 || size.0 > max_dimension || size.1 > max_dimension {
        return Err(refuse());
    }
    let row = size.0.checked_mul(8).ok_or_else(refuse)?;
    let bytes = usize::try_from(row)
        .ok()
        .and_then(|row| row.checked_mul(usize::try_from(size.1).ok()?))
        .ok_or_else(refuse)?;
    Ok((bytes, row))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit wave 5 (upscalers M1, L3): an output the device cannot hold is a backend error (the
    /// frame then falls back), never an overflow or a panic; an ordinary one keeps its layout.
    #[test]
    fn an_output_past_the_device_limit_is_refused_before_any_allocation() {
        assert_eq!(
            output_zero_fill((1920, 1080), 8192).ok(),
            Some((1920 * 1080 * 8, 1920 * 8))
        );
        for size in [(0, 1080), (1920, 0), (8193, 16), (16, 8193), (u32::MAX, u32::MAX)] {
            assert!(
                matches!(output_zero_fill(size, 8192), Err(UpscaleError::Backend(_))),
                "{size:?}"
            );
        }
        assert!(matches!(
            output_zero_fill((u32::MAX, 2), u32::MAX),
            Err(UpscaleError::Backend(_))
        ));
    }

    #[test]
    fn without_the_binding_the_status_is_not_bundled_and_nothing_is_wanted() {
        if BINDING_BUILT {
            return;
        }
        let status = dlss_status();
        assert_eq!(status.runtime, VendorRuntime::NotBundled);
        assert!(!status.binding_built);
        assert_eq!(status.sdk_version, "none");
        assert!(!wanted());
        let binding = DlssNgx::unattached();
        assert_eq!(binding.runtime(), VendorRuntime::NotBundled);
        assert!(binding.vulkan_extensions().is_err());
        assert_eq!(
            binding
                .optimal_settings((1920, 1080), UpscaleQuality::Quality)
                .err(),
            Some(UpscaleError::RuntimeUnavailable {
                reason: reason::SDK_NOT_BUNDLED
            })
        );
    }

    #[test]
    fn the_found_state_names_its_own_reason() {
        assert_eq!(VendorRuntime::Found.name(), "found");
        assert_eq!(
            VendorRuntime::Found.blocker(),
            Some(reason::RUNTIME_NOT_INITIALIZED)
        );
        assert_eq!(
            not_ready(VendorRuntime::Found),
            UpscaleError::RuntimeUnavailable {
                reason: reason::RUNTIME_NOT_INITIALIZED
            }
        );
        // `Ready` has no blocker; a binding that is not live still refuses with a code.
        assert_eq!(
            not_ready(VendorRuntime::Ready),
            UpscaleError::RuntimeUnavailable {
                reason: reason::SDK_NOT_BUNDLED
            }
        );
    }

    #[test]
    fn the_feature_mode_is_the_mode_nearest_the_frame_scale() {
        let display = (2560, 1440);
        for mode in UpscaleQuality::ALL {
            assert_eq!(
                UpscaleQuality::nearest(mode.render_size(display), display),
                mode,
                "{mode:?}"
            );
        }
        // A profile's 0.67 scale is Quality; 0.6 Balanced; 0.45 Performance; 0.2 Ultra Performance;
        // a render larger than the display (supersampling) Native.
        let at = |scale: f32| ((2560.0 * scale) as u32, (1440.0 * scale) as u32);
        assert_eq!(
            UpscaleQuality::nearest(at(0.67), display),
            UpscaleQuality::Quality
        );
        assert_eq!(
            UpscaleQuality::nearest(at(0.6), display),
            UpscaleQuality::Balanced
        );
        assert_eq!(
            UpscaleQuality::nearest(at(0.45), display),
            UpscaleQuality::Performance
        );
        assert_eq!(
            UpscaleQuality::nearest(at(0.2), display),
            UpscaleQuality::UltraPerformance
        );
        assert_eq!(
            UpscaleQuality::nearest((3000, 1700), display),
            UpscaleQuality::Native
        );
        assert_eq!(UpscaleQuality::nearest((0, 0), (0, 0)), UpscaleQuality::Native);
        // The larger-ratio axis decides.
        assert_eq!(
            UpscaleQuality::nearest((2560, 720), display),
            UpscaleQuality::Performance
        );
    }

    /// ADR-2679: the codes `regia_dlss` answers map to one runtime state each. A GTX 1080 is NGX's
    /// `feature_not_supported` at the device hook (measured, ADR-4937), another refused GPU's answer
    /// can be `unavailable`; a Vulkan failure during init is not the GPU's fault.
    #[test]
    fn binding_codes_map_to_the_runtime_state_they_mean() {
        for (code, runtime, reason) in [
            ("not_linked", VendorRuntime::NotBundled, reason::SDK_NOT_BUNDLED),
            (
                "unavailable",
                VendorRuntime::ReportsUnsupported,
                reason::RUNTIME_REPORTS_UNSUPPORTED,
            ),
            (
                "feature_not_supported",
                VendorRuntime::ReportsUnsupported,
                reason::RUNTIME_REPORTS_UNSUPPORTED,
            ),
            (
                "needs_newer_driver",
                VendorRuntime::NeedsNewerDriver,
                reason::NEEDS_NEWER_DRIVER,
            ),
            (
                "vulkan_error",
                VendorRuntime::InitFailed,
                reason::RUNTIME_INIT_FAILED,
            ),
            (
                "wgpu_error",
                VendorRuntime::InitFailed,
                reason::RUNTIME_INIT_FAILED,
            ),
            ("poisoned", VendorRuntime::InitFailed, reason::RUNTIME_INIT_FAILED),
            (
                "platform_error",
                VendorRuntime::InitFailed,
                reason::RUNTIME_INIT_FAILED,
            ),
            (
                "unable_to_initialize_feature",
                VendorRuntime::InitFailed,
                reason::RUNTIME_INIT_FAILED,
            ),
        ] {
            assert_eq!(runtime_of_code(code), runtime, "{code}");
            assert_eq!(runtime.blocker(), Some(reason), "{code}");
        }
    }

    /// ADR-4937: a failed NGX step leaves the process status in the state its code means, so the
    /// agent's row and the frame agree. A GTX 1080's hook answer (`feature_not_supported`, measured
    /// on 580.173.02) turns `found` into `reports_unsupported`; a Vulkan failure is `init_failed`.
    #[test]
    fn a_recorded_failure_moves_the_process_state_with_its_code() {
        for (code, runtime) in [
            ("feature_not_supported", VendorRuntime::ReportsUnsupported),
            ("unavailable", VendorRuntime::ReportsUnsupported),
            ("needs_newer_driver", VendorRuntime::NeedsNewerDriver),
            ("vulkan_error", VendorRuntime::InitFailed),
        ] {
            let mut status = DlssStatus {
                runtime: VendorRuntime::Found,
                binding_built: true,
                sdk_version: "test",
                library: Some(PathBuf::from("/abs/libnvidia-ngx-dlss.so")),
                code: None,
                instance_extensions: Vec::new(),
                device_extensions: Vec::new(),
            };
            assert_eq!(record_failure(&mut status, code), runtime, "{code}");
            assert_eq!((status.runtime, status.code), (runtime, Some(code)), "{code}");
        }
    }

    /// The real error type gives the codes the table above is written against.
    #[cfg(feature = "dlss")]
    #[test]
    fn the_dlss_errors_answer_the_codes_the_table_maps() {
        use regia_dlss::DlssError as E;
        assert_eq!(runtime_of(&E::NotLinked), VendorRuntime::NotBundled);
        assert_eq!(runtime_of(&E::Unavailable), VendorRuntime::ReportsUnsupported);
        assert_eq!(runtime_of(&E::NeedsNewerDriver), VendorRuntime::NeedsNewerDriver);
        assert_eq!(runtime_of(&E::Vulkan("lost".into())), VendorRuntime::InitFailed);
        assert_eq!(runtime_of(&E::Wgpu("lost".into())), VendorRuntime::InitFailed);
        assert_eq!(runtime_of(&E::Poisoned), VendorRuntime::InitFailed);
        assert_eq!(
            runtime_of(&E::Ngx {
                call: "init",
                failure: regia_dlss::NgxFailure::FeatureNotSupported,
            }),
            VendorRuntime::ReportsUnsupported
        );
    }

    fn feature(display: (u32, u32), quality: UpscaleQuality) -> FeatureDesc {
        FeatureDesc {
            display_size: display,
            quality,
            flags: super::super::FeatureFlags::REGIA,
        }
    }

    /// A scale between two modes keeps the live feature while it accepts the size: a
    /// dynamic-resolution step must not recreate the feature and drop its history.
    #[test]
    fn a_scale_the_live_feature_accepts_keeps_its_mode() {
        let display = (2560, 1440);
        let settings = OptimalSettings {
            render_size: (1707, 960),
            render_min: (1280, 720),
            render_max: (2560, 1440),
            sharpness: 0.0,
        };
        let live = Some((feature(display, UpscaleQuality::Quality), settings));
        // Inside the range, nearest says Performance; the feature stays.
        assert_eq!(
            feature_mode(UpscaleQuality::Performance, live, display, (1300, 731)),
            UpscaleQuality::Quality
        );
        // Below it, the feature cannot take the size.
        assert_eq!(
            feature_mode(UpscaleQuality::Performance, live, display, (1200, 675)),
            UpscaleQuality::Performance
        );
        // A new display is a new feature.
        assert_eq!(
            feature_mode(UpscaleQuality::Balanced, live, (1920, 1080), (1300, 731)),
            UpscaleQuality::Balanced
        );
        assert_eq!(
            feature_mode(UpscaleQuality::Balanced, None, display, (1300, 731)),
            UpscaleQuality::Balanced
        );
    }

    fn slot_with(config: super::super::scripted::ScriptConfig) -> FrameVendor {
        let vendor = config.vendor;
        let mut slot = FrameVendor::default();
        slot.select(Some(vendor));
        let binding: Box<dyn GpuBinding> = Box::new(super::super::scripted::ScriptedVendor::new(config));
        slot.backend = Some(VendorBackend::new(binding, UpscaleQuality::Quality));
        slot
    }

    fn lab_quality(mode: Option<UpscaleQuality>, permille: u16) -> crate::FrameQuality {
        crate::FrameQuality {
            upscale_quality: mode,
            render_scale_permille: permille,
            ..crate::FrameQuality::LAB
        }
    }

    /// ADR-2679: the slot's size selection with a range unlike any built-in's: preferred 77 %,
    /// accepted 61 % to 91 %. No built-in mode sits at those scales.
    #[test]
    fn the_slot_renders_at_the_runtimes_preferred_size_and_stays_in_its_range() {
        let display = (1000, 500);
        let config = super::super::scripted::ScriptConfig {
            range_permille: Some((770, 610, 910)),
            ..super::super::scripted::ScriptConfig::ready(VendorUpscaler::Xess)
        };
        let mut slot = slot_with(config);
        let mode = Some(UpscaleQuality::Quality);
        let at = |slot: &mut FrameVendor, mode, permille: u16| {
            let q = lab_quality(mode, permille);
            let requested = q.scaled_extent(display.0, display.1);
            slot.extent_with_backend(requested, display, &q)
        };
        // The mode's own scale is the preferred size, not the mode's 667 ‰.
        assert_eq!(at(&mut slot, mode, 667), (770, 385));
        assert_eq!(slot.report().render_preferred, Some((770, 385)));
        assert_eq!(slot.report().render_range, Some(((610, 305), (910, 455))));
        // Dynamic resolution moves from there, continuously, and stops at min and max.
        assert_eq!(at(&mut slot, mode, 600), (693, 346));
        assert_eq!(at(&mut slot, mode, 100), (610, 305));
        assert_eq!(at(&mut slot, mode, 1000), (910, 455));
        // A custom scale with no mode keeps its own size inside the range.
        assert_eq!(at(&mut slot, None, 800), (800, 400));
        assert_eq!(at(&mut slot, None, 500), (610, 305));
        assert_eq!(at(&mut slot, None, 1000), (910, 455));

        // A static range: every scale renders the one size.
        let config = super::super::scripted::ScriptConfig {
            range_permille: Some((770, 770, 770)),
            ..super::super::scripted::ScriptConfig::ready(VendorUpscaler::Xess)
        };
        let mut slot = slot_with(config);
        for permille in [100, 500, 667, 1000] {
            assert_eq!(at(&mut slot, mode, permille), (770, 385), "{permille}");
            assert_eq!(at(&mut slot, None, permille), (770, 385), "{permille}");
        }
    }

    /// When the range becomes unavailable the report drops it with the preferred size, instead of
    /// keeping the last range in `render.get_vendor_upscaler` (corr-low-render,
    /// corr-render-frame-upscaler finding 12).
    #[test]
    fn a_range_that_becomes_unavailable_leaves_the_report() {
        let display = (1000, 500);
        let q = lab_quality(Some(UpscaleQuality::Quality), 667);
        let requested = q.scaled_extent(display.0, display.1);
        let mut slot = slot_with(super::super::scripted::ScriptConfig {
            range_permille: Some((770, 610, 910)),
            ..super::super::scripted::ScriptConfig::ready(VendorUpscaler::Xess)
        });
        slot.extent_with_backend(requested, display, &q);
        assert!(slot.report().render_range.is_some());
        let gone: Box<dyn GpuBinding> = Box::new(super::super::scripted::ScriptedVendor::new(
            super::super::scripted::ScriptConfig {
                runtime: VendorRuntime::NotFound,
                ..super::super::scripted::ScriptConfig::ready(VendorUpscaler::Xess)
            },
        ));
        slot.backend = Some(VendorBackend::new(gone, UpscaleQuality::Quality));
        assert_eq!(slot.extent_with_backend(requested, display, &q), requested);
        assert_eq!(slot.report().render_preferred, None);
        assert_eq!(
            slot.report().render_range,
            None,
            "a stale range stayed in the report"
        );
    }

    /// A frame the vendor cannot run renders what it asked for, so it is the built-in's frame.
    #[test]
    fn a_binding_that_cannot_answer_keeps_the_frames_own_size() {
        let display = (1000, 500);
        let q = lab_quality(Some(UpscaleQuality::Quality), 667);
        let requested = q.scaled_extent(display.0, display.1);
        for runtime in [
            VendorRuntime::NotBundled,
            VendorRuntime::NotFound,
            VendorRuntime::Found,
            VendorRuntime::ReportsUnsupported,
            VendorRuntime::NeedsNewerDriver,
            VendorRuntime::InitFailed,
        ] {
            let config = super::super::scripted::ScriptConfig {
                runtime,
                range_permille: Some((770, 610, 910)),
                ..super::super::scripted::ScriptConfig::ready(VendorUpscaler::Dlss)
            };
            let mut slot = slot_with(config);
            assert_eq!(
                slot.extent_with_backend(requested, display, &q),
                requested,
                "{runtime:?}"
            );
            assert_eq!(slot.report().render_preferred, None, "{runtime:?}");
        }
        // The stub DLSS and XeSS bindings have no answer either.
        for binding in [
            Box::new(crate::upscaler::vendor::DlssSdk) as Box<dyn GpuBinding>,
            Box::new(crate::upscaler::vendor::XessSdk),
        ] {
            let mut slot = FrameVendor::default();
            slot.select(Some(VendorUpscaler::Xess));
            slot.backend = Some(VendorBackend::new(binding, UpscaleQuality::Quality));
            assert_eq!(slot.extent_with_backend(requested, display, &q), requested);
        }
        // A runtime answer that is not usable is not used.
        let bogus = super::super::scripted::ScriptConfig {
            range_permille: Some((900, 950, 300)),
            ..super::super::scripted::ScriptConfig::ready(VendorUpscaler::Xess)
        };
        assert_eq!(
            slot_with(bogus).extent_with_backend(requested, display, &q),
            requested
        );
        // No selection and non-vendor frames: untouched, without asking a device for anything.
        let mut none = FrameVendor::default();
        assert_eq!(none.extent_with_backend(requested, display, &q), requested);
    }

    /// An empty recorded hook list is not a missing extension when NGX's own list for this
    /// device is empty. A negative answer still blocks, including when another device's hook
    /// recorded names. Without a binding, only a non-empty recorded list counts as enabled.
    #[test]
    fn empty_recorded_list_is_ready_when_ngx_wants_nothing() {
        assert!(dlss_vulkan_extensions_flag(Some(true), false));
        assert!(!dlss_vulkan_extensions_flag(Some(false), true));
        assert!(!dlss_vulkan_extensions_flag(None, false));
        assert!(dlss_vulkan_extensions_flag(None, true));
    }

    #[test]
    fn a_frame_without_a_selection_is_left_to_the_built_in() {
        let mut slot = FrameVendor::default();
        assert_eq!(slot.selected(), None);
        slot.select(Some(VendorUpscaler::Dlss));
        assert_eq!(slot.report().selected, Some(VendorUpscaler::Dlss));
        slot.select(None);
        assert_eq!(slot.report(), VendorFrameReport::default());
    }
}
