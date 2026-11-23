// SPDX-License-Identifier: MPL-2.0
//! Intel XeSS Super Resolution on Windows Vulkan (ADR-4656).
//!
//! Build with `sdk` and an external `XESS_SDK` (SDK bundle 3.0.2). Without it every
//! runtime operation returns `not_linked`. No SDK headers, binaries or generated bindings ship
//! in this crate. Load an absolute DLL path with [`Runtime::load`], create the instance and
//! device through that runtime, then create a [`Context`] on that device. The runtime queries
//! both Vulkan extensions and feature flags before device creation.
//!
//! Commands retain their context until submission and their views through GPU completion.
//! The preceding encoder transitions and
//! retains the input textures; submit it immediately before the XeSS buffer. Initialize the
//! output with wgpu before evaluating, so wgpu's lazy initialization cannot erase the raw writes.
use std::path::Path;

#[cfg(regia_xess_linked)]
mod native;
#[cfg(not(regia_xess_linked))]
mod stub;
#[cfg(regia_xess_linked)]
use native as imp;
#[cfg(not(regia_xess_linked))]
use stub as imp;

/// Whether this build generated the Windows SDK binding.
pub const LINKED: bool = cfg!(regia_xess_linked);
/// External SDK bundle used by this adapter; the SR runtime has a separate version.
pub const SDK_VERSION: &str = if LINKED { "3.0.2" } else { "none" };

/// XeSS preset; input sizes are queried from the runtime, never inferred from these values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// Native-resolution temporal anti-aliasing.
    Native,
    /// Quality preset.
    Quality,
    /// Balanced preset.
    Balanced,
    /// Performance preset.
    Performance,
    /// Ultra-performance preset.
    UltraPerformance,
}
impl Quality {
    /// Public SDK quality identifier.
    #[must_use]
    pub const fn sdk_value(self) -> u32 {
        match self {
            Self::Native => 106,
            Self::Quality => 103,
            Self::Balanced => 102,
            Self::Performance => 101,
            Self::UltraPerformance => 100,
        }
    }
}

/// Super Resolution ABI version returned by the loaded runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    /// ABI major version.
    pub major: u16,
    /// Compatible minor version (90+ is refused like a development patch).
    pub minor: u16,
    /// Patch version (90+ denotes development builds).
    pub patch: u16,
}
impl Version {
    /// Whether the adapter understands this stable ABI (XeSS SR 2.x): major 2, with a minor and
    /// a patch below 90 (the SDK's development-build range; a minor of 90 or more is refused
    /// too, as a build no stable 2.x release ships).
    #[must_use]
    pub const fn supported(self) -> bool {
        self.major == 2 && self.minor < 90 && self.patch < 90
    }
}

/// A recoverable binding or SDK failure, with a protocol-safe [`Error::code`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// SDK binding was not built.
    #[error("XeSS SDK binding not linked")]
    NotLinked,
    /// The supplied object uses another graphics API.
    #[error("XeSS requires Vulkan")]
    NotVulkan,
    /// Invalid sizes, texture usage, format, scalar or device identity.
    #[error("invalid XeSS input")]
    InvalidInput,
    /// No library exists at the absolute configured path.
    #[error("XeSS runtime not found at an absolute path")]
    RuntimeNotFound,
    /// The loaded ABI is not supported.
    #[error("unsupported XeSS runtime ABI")]
    UnsupportedVersion,
    /// Device was not prepared with XeSS's requirements.
    #[error("device was not created through the XeSS hook")]
    DeviceNotPrepared,
    /// Vendor API returned a failing result.
    #[error("XeSS {call} returned {result}")]
    Sdk {
        /// Public SDK function name.
        call: &'static str,
        /// SDK result number.
        result: i32,
    },
    /// Loader, Vulkan, synchronization or wgpu error.
    #[error("XeSS backend: {0}")]
    Backend(String),
}
impl Error {
    /// Stable lowercase error identifier for logs and the agent protocol.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotLinked => "not_linked",
            Self::NotVulkan => "not_vulkan",
            Self::InvalidInput => "invalid_input",
            Self::RuntimeNotFound => "runtime_not_found",
            Self::UnsupportedVersion => "unsupported_version",
            Self::DeviceNotPrepared => "device_not_prepared",
            Self::Sdk { result: -1, .. } => "unsupported_device",
            Self::Sdk { result: -2, .. } => "needs_newer_driver",
            Self::Sdk { result: -4, .. } => "invalid_input",
            Self::Sdk { result: -5, .. } => "out_of_gpu_memory",
            Self::Sdk { result: -7 | -10, .. } => "unsupported",
            Self::Sdk { .. } => "sdk_error",
            Self::Backend(_) => "backend_error",
        }
    }
}

/// How one of XeSS's app-owned temporary heaps was zeroed before XeSS used it (ADR-4907,
/// ADR-7820). The first match of host write, driver zeroing and device fill is taken; the
/// diagnostic `REGIA_XESS_HEAP_ZEROING` (`host`, `driver` or `fill`) forces one method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeapZeroing {
    /// Device-local, host-visible memory written with zeros by the CPU before initialization
    /// (GPUs whose host-visible device-local memory takes images, such as Intel's).
    HostWrite,
    /// Device-local memory the driver allocated zeroed (`VK_EXT_zero_initialize_device_memory`,
    /// enabled by [`Runtime::request_device`] where the GPU offers it).
    DriverZeroed,
    /// Device-local memory filled with zeros by the GPU, through a transfer buffer bound over
    /// the whole heap, at the start of the first [`Context::evaluate`]'s commands. That first
    /// command buffer must be submitted.
    DeviceFill,
}
impl HeapZeroing {
    /// Stable lowercase identifier for logs and evidence.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::HostWrite => "host_write",
            Self::DriverZeroed => "driver_zeroed",
            Self::DeviceFill => "device_fill",
        }
    }
}

/// Runtime-recommended input size and permitted dynamic-resolution range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptimalSettings {
    /// Preferred input size.
    pub render: [u32; 2],
    /// Minimum input size.
    pub min: [u32; 2],
    /// Maximum input size.
    pub max: [u32; 2],
}

/// Immutable feature initialization options. Motion vectors are always at input resolution.
#[derive(Debug, Clone, Copy)]
pub struct CreateDesc {
    /// Output dimensions.
    pub display: [u32; 2],
    /// Runtime quality preset.
    pub quality: Quality,
    /// Whether smaller depth values are farther from the camera.
    pub depth_inverted: bool,
    /// Whether color is HDR (normally true in Regia).
    pub hdr: bool,
    /// Whether motion includes projection jitter.
    pub motion_jittered: bool,
}

/// Borrowed full 2D, single-mip, single-layer frame views on the context device.
/// Motion is current-to-previous, scaled to input pixels.
pub struct EvalInputs<'a> {
    /// HDR color, RGBA16Float.
    pub color: &'a wgpu::TextureView,
    /// Standard or inverted depth as configured at initialization.
    pub depth: &'a wgpu::TextureView,
    /// RG16Float or RGBA16Float motion vectors; RGBA input is converted from its XY channels.
    pub motion: &'a wgpu::TextureView,
    /// Display-sized RGBA16Float storage output, initialized through wgpu first.
    pub output: &'a wgpu::TextureView,
    /// Projection offset in input pixels, positive y down.
    pub jitter: [f32; 2],
    /// Multiply the motion texture by this scale to obtain input pixels.
    pub motion_scale: [f32; 2],
    /// Post exposure divided by pre-exposure, finite and positive.
    pub exposure: f32,
    /// Active input dimensions.
    pub render_size: [u32; 2],
    /// Clear accumulation on first frame, cuts and discontinuities.
    pub reset: bool,
}

/// Loaded DLL and its validated ABI. Clones keep the library resident.
#[derive(Clone)]
pub struct Runtime(imp::Runtime);
impl Runtime {
    /// Load an absolute DLL path. Never searches the working directory.
    ///
    /// # Errors
    /// `not_linked`, `runtime_not_found`, `unsupported_version`, or a loader/SDK error.
    pub fn load(path: &Path) -> Result<Self, Error> {
        imp::Runtime::load(path).map(Self)
    }
    /// Loaded Super Resolution version.
    #[must_use]
    pub fn version(&self) -> Version {
        self.0.version()
    }
    /// Create a Vulkan instance with all requirements queried from XeSS.
    ///
    /// # Errors
    /// Runtime requirement or Vulkan initialization failure.
    pub fn create_instance(
        &self,
        desc: &wgpu::InstanceDescriptor,
    ) -> Result<(wgpu::Instance, Vec<String>), Error> {
        self.0.create_instance(desc)
    }
    /// Create a device with wgpu's requirements plus XeSS's full feature chain.
    ///
    /// # Errors
    /// Unsupported adapter, missing instance requirements or device creation failure.
    pub fn request_device(
        &self,
        adapter: &wgpu::Adapter,
        desc: &wgpu::DeviceDescriptor<'_>,
    ) -> Result<(wgpu::Device, wgpu::Queue, Vec<String>), Error> {
        self.0.request_device(adapter, desc)
    }
    /// Create an uninitialized context on a device prepared by this adapter.
    ///
    /// # Errors
    /// Unprepared device, unsupported hardware/driver or an SDK failure.
    pub fn context(&self, device: &wgpu::Device) -> Result<Context, Error> {
        self.0.context(device).map(Context)
    }
}

/// One XeSS history and its resources; retained by outstanding command leases.
pub struct Context(imp::Context);
impl Context {
    /// Query native runtime sizes before initialization.
    ///
    /// # Errors
    /// Invalid output size or SDK failure.
    pub fn optimal_settings(&self, display: [u32; 2], quality: Quality) -> Result<OptimalSettings, Error> {
        if display.contains(&0) {
            return Err(Error::InvalidInput);
        }
        self.0.optimal_settings(display, quality)
    }
    /// Initialize exactly once; create another context on resize or quality changes.
    ///
    /// # Errors
    /// Invalid descriptor, repeated initialization or SDK allocation/compilation failure.
    pub fn initialize(&mut self, desc: CreateDesc) -> Result<(), Error> {
        if desc.display.contains(&0) {
            return Err(Error::InvalidInput);
        }
        self.0.initialize(desc)
    }
    /// Whether [`Self::initialize`] gave XeSS app-owned zero-initialized temporary heaps, which
    /// make its output repeatable for identical inputs (ADR-4907). `false` before
    /// initialization, in stub builds, and where the SDK allocated its own (a warning is logged).
    #[must_use]
    pub fn zeroed_temp_heaps(&self) -> bool {
        self.0.zeroed_temp_heaps()
    }
    /// How each zeroed temporary heap was zeroed, `[buffer heap, texture heap]` (ADR-7820);
    /// `None` exactly when [`Self::zeroed_temp_heaps`] is `false`.
    #[must_use]
    pub fn temp_heap_zeroing(&self) -> Option<[HeapZeroing; 2]> {
        self.0.temp_heap_zeroing()
    }
    /// Record one frame. The caller submits `before` immediately before the returned commands.
    ///
    /// # Errors
    /// Invalid frame inputs or device identity, or an SDK failure.
    pub fn evaluate(
        &mut self,
        device: &wgpu::Device,
        before: &mut wgpu::CommandEncoder,
        inputs: &EvalInputs<'_>,
    ) -> Result<Commands, Error> {
        self.0
            .evaluate(device, before, inputs)
            .map(|(buffer, lease)| Commands {
                buffer,
                lease: Lease { _inner: lease },
            })
    }
}

/// Recorded XeSS commands and the lifetime of their context.
#[must_use = "XeSS commands do nothing until submitted"]
pub struct Commands {
    buffer: wgpu::CommandBuffer,
    lease: Lease,
}
impl Commands {
    /// Submit the transition/input buffer followed by XeSS while retaining its context.
    pub fn submit(self, queue: &wgpu::Queue, before: wgpu::CommandBuffer) -> wgpu::SubmissionIndex {
        // The lease lives to the end of this scope, after submission, including in stub builds.
        let Self {
            buffer,
            lease: _lease,
        } = self;
        queue.submit([before, buffer])
    }
    /// Split for an engine frame. Keep the lease alive until after this buffer is submitted
    /// immediately after its transition buffer in the same `Queue::submit` batch; otherwise
    /// dropping the lease or completing the earlier batch may release GPU resources too soon.
    pub fn into_parts(self) -> (wgpu::CommandBuffer, Lease) {
        (self.buffer, self.lease)
    }
}
/// Keeps a recorded context alive until its command buffer has been submitted or discarded.
///
/// Dropping the last lease of a context that was already dropped destroys the context, and
/// that waits for the device to go idle (`Device::poll` with `wait_indefinitely`). Do not drop
/// it inside a wgpu callback (`map_async`, `on_submitted_work_done`): the wait would re-enter
/// `poll` from within `poll`.
pub struct Lease {
    _inner: imp::Lease,
}

/// Whether a device was created with the complete XeSS feature chain by this crate.
#[must_use]
pub fn device_prepared(device: &wgpu::Device) -> bool {
    imp::device_prepared(device)
}

#[cfg(any(regia_xess_linked, test))]
mod worker;

#[cfg(regia_xess_linked)]
mod motion;

#[cfg(any(regia_xess_linked, test))]
mod retain;

#[cfg(any(regia_xess_linked, test))]
mod pacing;

// Without the SDK binding only the lavapipe tests use this module (ADR-7820).
#[cfg(any(regia_xess_linked, all(test, any(target_os = "linux", target_os = "windows"))))]
#[cfg_attr(
    not(regia_xess_linked),
    allow(
        dead_code,
        reason = "production items are used by the SDK-linked native module only"
    )
)]
mod zeroing;

#[cfg(test)]
mod version_tests {
    use super::Version;

    /// The documented range (corr-xess-dlss-xr LOW 8): major 2, minor and patch below 90.
    #[test]
    fn supported_takes_stable_2x_and_refuses_development_numbers() {
        let v = |major, minor, patch| Version { major, minor, patch };
        assert!(v(2, 0, 0).supported());
        assert!(v(2, 89, 89).supported());
        assert!(!v(2, 90, 0).supported(), "minor 90+");
        assert!(!v(2, 1, 90).supported(), "patch 90+");
        assert!(!v(1, 3, 0).supported());
        assert!(!v(3, 0, 0).supported());
    }
}
