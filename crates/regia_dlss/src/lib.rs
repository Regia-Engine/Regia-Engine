// SPDX-License-Identifier: MPL-2.0
//! NVIDIA DLSS Super Resolution through NGX on wgpu's Vulkan backend (ADR-2002).
//!
//! The renderer reaches DLSS only through `regia_render::upscaler::vendor` (the vendor seam,
//! ADR-1150); this crate is the FFI underneath it. Everything here takes wgpu objects, so the
//! seam never sees a raw Vulkan handle.
//!
//! **Two builds, one API.** With the `sdk` feature and `DLSS_SDK` set at build time (an absolute
//! path to NVIDIA's DLSS SDK checkout, never committed), on Linux or Windows, the build script
//! links the static NGX library and [`LINKED`] is `true`. Otherwise every call returns
//! [`DlssError::NotLinked`]: the engine builds everywhere (Android, macOS, a machine without the
//! SDK) and falls back to its own upscalers. [`LINKED`] is also `true` for the check stub
//! (`REGIA_DLSS_CHECK_STUB=1`), which compiles `ngx.rs` and links no NGX library. Android and
//! macOS never link, even when `DLSS_SDK` is set (`no NGX for {target}`). A failed link falls
//! back to the stub with a cargo warning.
//!
//! **At run time** NGX loads the DLSS runtime (`libnvidia-ngx-dlss.so.<version>` on Linux,
//! `nvngx_dlss.dll` on Windows) from the directories in [`NgxConfig::library_dirs`]. A missing
//! runtime, a non-NVIDIA GPU or an old driver are errors with a stable [`DlssError::code`],
//! never a panic.
//!
//! The sequence a host follows:
//!
//! 1. [`create_instance`] and [`request_device`] instead of wgpu's own calls: they add the
//!    Vulkan extensions NGX asks for (when the system has them).
//! 2. [`Ngx::new`] once per device: initialises NGX and checks that DLSS is available.
//! 3. [`Ngx::optimal_settings`] for the render size, then [`SuperResolution::new`] per view,
//!    display size and mode.
//! 4. [`SuperResolution::evaluate`] each frame: transitions the inputs on the host's encoder
//!    and returns [`DlssCommands`], submitted right after that encoder ([`DlssCommands::submit`]);
//!    its lease keeps the feature alive until then.
//!
//! Parts of the binding (instance and device extension registration, feature info, the
//! evaluate parameter block) are adapted from the open-source `dlss_wgpu` crate, MIT OR
//! Apache-2.0 (`docs/provenance.md`, ADR-2002); the NGX entry points are NVIDIA's.

#![cfg_attr(regia_dlss_linked, allow(unsafe_code))]

use std::path::PathBuf;

#[cfg(any(test, regia_dlss_linked))]
mod lifecycle;
#[cfg(regia_dlss_linked)]
mod ngx;
#[cfg(regia_dlss_linked)]
use ngx as imp;
#[cfg(not(regia_dlss_linked))]
mod stub;
#[cfg(not(regia_dlss_linked))]
use stub as imp;

/// Whether this build compiled the NGX sources (`cfg(regia_dlss_linked)`).
///
/// True after a successful SDK link, and also for the check stub (`REGIA_DLSS_CHECK_STUB=1`),
/// which links nothing. False for the stub: the `sdk` feature off, `DLSS_SDK` unset or unusable,
/// or a target with no NGX library (Android and macOS).
pub const LINKED: bool = cfg!(regia_dlss_linked);

/// The version string the build script recorded.
///
/// * `none` — the stub (no SDK link).
/// * `check-stub` — `REGIA_DLSS_CHECK_STUB=1` (NGX sources compiled, nothing linked).
/// * a dotted suffix — Linux, from `libnvidia-ngx-dlss.so.<version>` (for example `310.9.1`).
/// * `windows` — the Windows redistributable is named `nvngx_dlss.dll`, which has no version in
///   the file name.
/// * `unknown` — a linked build whose `lib/<platform>/rel` directory has neither of those names.
pub const SDK_VERSION: &str = env!("REGIA_DLSS_SDK_VERSION");

/// Regia Engine's NGX project id (a custom-engine GUID; NGX keys its per-application settings
/// and over-the-air updates on it).
pub const PROJECT_ID: &str = "8f0a6d3e-5c41-4b7e-9e2d-2b6c1f9a7d54";

/// How NGX is initialised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NgxConfig {
    /// The project id ([`PROJECT_ID`]).
    pub project_id: String,
    /// The engine version NGX records.
    pub engine_version: String,
    /// Directories NGX loads the DLSS runtime from, in order.
    pub library_dirs: Vec<PathBuf>,
    /// Where NGX may write its logs and caches.
    pub data_dir: PathBuf,
}

impl NgxConfig {
    /// Regia Engine's project id and version, the given runtime directories and the system
    /// temporary directory for NGX's data.
    #[must_use]
    pub fn new(library_dirs: Vec<PathBuf>) -> Self {
        Self {
            project_id: PROJECT_ID.to_owned(),
            engine_version: env!("CARGO_PKG_VERSION").to_owned(),
            library_dirs,
            data_dir: std::env::temp_dir(),
        }
    }
}

/// DLSS performance/quality mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PerfQuality {
    /// Ratio 1: anti-aliasing only.
    Dlaa,
    /// Ratio 1.5.
    Quality,
    /// Ratio about 1.72.
    Balanced,
    /// Ratio 2.
    Performance,
    /// Ratio 3.
    UltraPerformance,
}

/// How the inputs are encoded: DLSS creation flags. A change needs a new [`SuperResolution`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CreateFlags {
    /// The colour input is scene-referred HDR.
    pub hdr: bool,
    /// Depth is reverse-Z (1 at the near plane).
    pub depth_inverted: bool,
    /// Motion vectors are at the render size.
    pub motion_low_res: bool,
    /// Motion vectors include the jitter difference.
    pub motion_jittered: bool,
    /// Forwarded to NGX as the auto-exposure creation flag.
    ///
    /// The feature does not store this flag. [`SuperResolution::evaluate`] still checks
    /// `inputs.exposure` when it is `Some` (it must be 1×1 `R32Float`) and still passes that
    /// texture. A 2×2 exposure, or any size other than 1×1, fails the call even when this flag
    /// was set at creation.
    pub auto_exposure: bool,
}

/// NGX's answer for a display size and mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OptimalSettings {
    /// Recommended render size.
    pub render: [u32; 2],
    /// Smallest render size the feature accepts.
    pub min: [u32; 2],
    /// Largest render size the feature accepts.
    pub max: [u32; 2],
    /// Recommended sharpness (0 in current runtimes: DLSS no longer sharpens).
    pub sharpness: f32,
}

/// Why NGX refused a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NgxFailure {
    /// Not supported by this system, GPU or API.
    FeatureNotSupported,
    /// The graphics API or the OS failed.
    PlatformError,
    /// An identical feature already exists.
    FeatureAlreadyExists,
    /// No feature for the handle.
    FeatureNotFound,
    /// A parameter was wrong or missing.
    InvalidParameter,
    /// A scratch buffer was too small.
    ScratchBufferTooSmall,
    /// NGX is not initialised.
    NotInitialized,
    /// An input had an unsupported format.
    UnsupportedInputFormat,
    /// An output lacks storage access.
    RwFlagMissing,
    /// A required input is missing.
    MissingInput,
    /// The feature could not start (usually: the runtime library was not found).
    UnableToInitializeFeature,
    /// The driver or the runtime is too old. The wrappers in this crate report it as
    /// [`DlssError::NeedsNewerDriver`] (`needs_newer_driver`), so it is not returned inside
    /// [`DlssError::Ngx`].
    OutOfDate,
    /// Out of GPU memory.
    OutOfGpuMemory,
    /// A buffer had an unsupported format.
    UnsupportedFormat,
    /// NGX cannot write to its data directory.
    UnableToWriteToAppDataPath,
    /// A parameter is unsupported.
    UnsupportedParameter,
    /// NVIDIA has restricted the feature for this application.
    Denied,
    /// Not implemented by this runtime or driver.
    NotImplemented,
    /// Another code.
    Unknown(u32),
}

impl NgxFailure {
    /// Stable snake_case name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FeatureNotSupported => "feature_not_supported",
            Self::PlatformError => "platform_error",
            Self::FeatureAlreadyExists => "feature_already_exists",
            Self::FeatureNotFound => "feature_not_found",
            Self::InvalidParameter => "invalid_parameter",
            Self::ScratchBufferTooSmall => "scratch_buffer_too_small",
            Self::NotInitialized => "not_initialized",
            Self::UnsupportedInputFormat => "unsupported_input_format",
            Self::RwFlagMissing => "rw_flag_missing",
            Self::MissingInput => "missing_input",
            Self::UnableToInitializeFeature => "unable_to_initialize_feature",
            Self::OutOfDate => "out_of_date",
            Self::OutOfGpuMemory => "out_of_gpu_memory",
            Self::UnsupportedFormat => "unsupported_format",
            Self::UnableToWriteToAppDataPath => "unable_to_write_to_app_data_path",
            Self::UnsupportedParameter => "unsupported_parameter",
            Self::Denied => "denied",
            Self::NotImplemented => "not_implemented",
            Self::Unknown(_) => "unknown",
        }
    }

    /// The failure of an NGX result code (`0xBAD0_00nn`), `None` for a success code. NGX's own
    /// failure test masks the top 12 bits (`code & 0xFFF0_0000 == 0xBAD0_0000`), so every code
    /// of that range is a failure: one this crate does not name is [`Self::Unknown`], never a
    /// success (audit upscalers M4: a `0xFFFF_F000` mask read `0xBAD1_0001` as success).
    #[must_use]
    pub const fn from_code(code: u32) -> Option<Self> {
        if code & 0xFFF0_0000 != 0xBAD0_0000 {
            return None;
        }
        Some(match code & 0x000F_FFFF {
            1 => Self::FeatureNotSupported,
            2 => Self::PlatformError,
            3 => Self::FeatureAlreadyExists,
            4 => Self::FeatureNotFound,
            5 => Self::InvalidParameter,
            6 => Self::ScratchBufferTooSmall,
            7 => Self::NotInitialized,
            8 => Self::UnsupportedInputFormat,
            9 => Self::RwFlagMissing,
            10 => Self::MissingInput,
            11 => Self::UnableToInitializeFeature,
            12 => Self::OutOfDate,
            13 => Self::OutOfGpuMemory,
            14 => Self::UnsupportedFormat,
            15 => Self::UnableToWriteToAppDataPath,
            16 => Self::UnsupportedParameter,
            17 => Self::Denied,
            18 => Self::NotImplemented,
            _ => Self::Unknown(code),
        })
    }
}

/// A DLSS call failed. Never a panic: the engine reports the code and falls back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DlssError {
    /// This build has no NGX SDK ([`LINKED`] is false).
    #[error("this build does not link the NGX SDK")]
    NotLinked,
    /// The device or adapter is not on wgpu's Vulkan backend.
    #[error("DLSS needs the Vulkan backend")]
    NotVulkan,
    /// NGX loaded and reports DLSS unavailable on this GPU or system.
    #[error("NGX reports DLSS Super Resolution unavailable")]
    Unavailable,
    /// NGX asks for a newer driver.
    #[error("NGX asks for a newer GPU driver")]
    NeedsNewerDriver,
    /// NGX returned a failure.
    #[error("NGX call {call} failed: {}", failure.name())]
    Ngx {
        /// The NGX entry point.
        call: &'static str,
        /// What failed.
        failure: NgxFailure,
    },
    /// A Vulkan call failed.
    #[error("Vulkan: {0}")]
    Vulkan(String),
    /// wgpu refused a call (device or instance creation).
    #[error("wgpu: {0}")]
    Wgpu(String),
    /// A texture format NGX cannot take.
    #[error("texture format {0:?} is not a DLSS input format")]
    UnsupportedFormat(wgpu::TextureFormat),
    /// A lock was poisoned by a panic elsewhere.
    #[error("the NGX state lock was poisoned")]
    Poisoned,
}

impl DlssError {
    /// Stable snake_case code for logs and the agent protocol.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotLinked => "not_linked",
            Self::NotVulkan => "not_vulkan",
            Self::Unavailable => "unavailable",
            Self::NeedsNewerDriver => "needs_newer_driver",
            Self::Ngx { failure, .. } => failure.name(),
            Self::Vulkan(_) => "vulkan_error",
            Self::Wgpu(_) => "wgpu_error",
            Self::UnsupportedFormat(_) => "unsupported_format",
            Self::Poisoned => "poisoned",
        }
    }
}

/// The Vulkan extensions NGX added at instance or device creation.
pub type AddedExtensions = Vec<String>;

/// A wgpu instance on the Vulkan backend with the instance extensions DLSS needs added, and
/// their names. When the system lacks one of them the instance is created without the DLSS
/// extensions and the list is empty (DLSS then cannot run; nothing else changes).
///
/// # Errors
///
/// [`DlssError::NotLinked`] in the stub; NGX or Vulkan failures.
pub fn create_instance(
    descriptor: &wgpu::InstanceDescriptor,
    config: &NgxConfig,
) -> Result<(wgpu::Instance, AddedExtensions), DlssError> {
    imp::create_instance(descriptor, config)
}

/// A device and queue on a Vulkan `adapter` with the device extensions DLSS needs added, and
/// their names (empty when the adapter lacks one of them).
///
/// # Errors
///
/// [`DlssError::NotLinked`] in the stub, [`DlssError::NotVulkan`], NGX or wgpu failures.
pub fn request_device(
    adapter: &wgpu::Adapter,
    descriptor: &wgpu::DeviceDescriptor<'_>,
    config: &NgxConfig,
) -> Result<(wgpu::Device, wgpu::Queue, AddedExtensions), DlssError> {
    imp::request_device(adapter, descriptor, config)
}

/// Whether `device` was created with every Vulkan extension NGX asked for: `instance` enabled on
/// its instance and `device_wanted` on the device (the lists [`create_instance`] and
/// [`request_device`] returned). NGX must not initialise on a device the hooks did not create,
/// for example a second device of the process (audit wave 6, upscalers M2). Always `false` in
/// the stub and on a device of another backend.
#[must_use]
pub fn device_has_extensions(device: &wgpu::Device, instance: &[String], device_wanted: &[String]) -> bool {
    imp::device_has_extensions(device, instance, device_wanted)
}

/// Whether `device` and its instance enable every Vulkan extension NGX asks for on this device's
/// GPU, asked from NGX for this device. The gate before [`Ngx::new`] for a host with more than
/// one device: the lists [`create_instance`] and [`request_device`] return describe the device
/// they made, and an empty list there means both "none wanted" and "the system lacks one"
/// (audit 11-03, dlss M2 and L1).
///
/// # Errors
///
/// [`DlssError::NotLinked`] in the stub, [`DlssError::NotVulkan`] on another backend, or an NGX
/// failure while asking for the lists.
pub fn device_has_ngx_extensions(device: &wgpu::Device, config: &NgxConfig) -> Result<bool, DlssError> {
    imp::device_has_ngx_extensions(device, config)
}

/// NGX initialised on one device, with DLSS Super Resolution available.
pub struct Ngx(imp::Ngx);

impl std::fmt::Debug for Ngx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ngx").finish_non_exhaustive()
    }
}

impl Ngx {
    /// Initialises NGX on `device` (created by [`request_device`]) and checks that DLSS Super
    /// Resolution is available. NGX is initialised once per device in the process: while an
    /// `Ngx` of `device` is alive, this returns another handle to it (`config` is then not used),
    /// and NGX shuts down when the last handle (and the last feature) drops. A call while the
    /// device's last instance is shutting down waits for that shutdown; other devices never
    /// wait for it (audit 11-03, dlss M1 and M3).
    ///
    /// # Errors
    ///
    /// [`DlssError::NotLinked`], [`DlssError::NotVulkan`], [`DlssError::Unavailable`],
    /// [`DlssError::NeedsNewerDriver`] or an NGX failure (a missing runtime library is
    /// [`NgxFailure::UnableToInitializeFeature`] or [`NgxFailure::FeatureNotSupported`]).
    pub fn new(config: &NgxConfig, device: &wgpu::Device) -> Result<Self, DlssError> {
        imp::Ngx::new(config, device).map(Self)
    }

    /// Whether this NGX instance was initialised on `device`. NGX is initialised once per device
    /// and shut down once: a second renderer on the same device shares the first one's instance
    /// (audit wave 6, upscalers M1).
    #[must_use]
    pub fn is_on(&self, device: &wgpu::Device) -> bool {
        self.0.is_on(device)
    }

    /// The render size and range for `display` at `quality`.
    ///
    /// On a linked build, [`PerfQuality::Dlaa`] returns `render`, `min`, and `max` all equal to
    /// `display` and `sharpness` 0, without calling NGX. A display that contains 0 is returned
    /// the same way. Every other mode calls NGX; a render size from that call that contains 0
    /// is [`NgxFailure::UnsupportedParameter`]. The stub returns [`DlssError::NotLinked`] for
    /// every mode, including DLAA.
    ///
    /// # Errors
    ///
    /// [`DlssError::NotLinked`] on the stub. On a linked build, an NGX failure for a mode other
    /// than DLAA (the mode is not supported at that size, or NGX returned a render size that
    /// contains 0). DLAA does not return an error for a zero display.
    pub fn optimal_settings(
        &self,
        display: [u32; 2],
        quality: PerfQuality,
    ) -> Result<OptimalSettings, DlssError> {
        self.0.optimal_settings(display, quality)
    }

    /// Bytes of GPU memory DLSS holds.
    ///
    /// # Errors
    ///
    /// An NGX failure.
    pub fn vram_bytes(&self) -> Result<u64, DlssError> {
        self.0.vram_bytes()
    }
}

/// One frame's DLSS inputs. Callers pass single-mip, layer-0, 2D views. The size check measures
/// the parent texture's base level (`texture().width()` and `height()`), not the viewed mip or
/// array layer, and the NGX resource is described as that full size starting at mip 0, layer 0.
/// A view of a smaller mip, or of one layer of an array, is not rejected when the parent base
/// level is large enough. Inputs need `TEXTURE_BINDING`, the output `STORAGE_BINDING`.
pub struct EvalInputs<'a> {
    /// Jittered scene colour.
    pub color: &'a wgpu::TextureView,
    /// Scene depth.
    pub depth: &'a wgpu::TextureView,
    /// Motion vectors (red, green), scaled by `motion_scale` into render pixels to the previous
    /// frame.
    pub motion: &'a wgpu::TextureView,
    /// 1×1 `R32Float` exposure multiplier when `Some`. Checked and forwarded even when the
    /// feature was created with [`CreateFlags::auto_exposure`].
    pub exposure: Option<&'a wgpu::TextureView>,
    /// Reactive mask (red, 0–1): how much to trust this frame's colour (NGX's
    /// current-colour bias mask).
    pub reactive: Option<&'a wgpu::TextureView>,
    /// Transparency-and-composition mask (red, 0–1); reserved by current runtimes.
    pub transparency: Option<&'a wgpu::TextureView>,
    /// Display-size output.
    pub output: &'a wgpu::TextureView,
    /// Jitter in render pixels, the projection's offset, y down.
    pub jitter: [f32; 2],
    /// Factors from the motion texture's units to render pixels.
    pub motion_scale: [f32; 2],
    /// The part of the inputs this frame rendered.
    pub render_size: [u32; 2],
    /// Drop the history.
    pub reset: bool,
    /// Frame time in milliseconds, copied to NGX as `InFrameTimeDeltaInMsec`.
    ///
    /// The render frame's vendor path passes a fixed 16.667 ms
    /// (`UpscaleParams::new` in `regia_render`) and does not pass the measured frame delta.
    /// Other callers can set this field themselves.
    pub frame_time_ms: f32,
}

/// Rejects a parent texture smaller than `render_size` (NGX would read past that image and still
/// return success), an output parent smaller than the display size, and an exposure that is not
/// a 1×1 `R32Float` texture. The measurement is the parent texture's base level, not the view's
/// mip or array layer.
fn check_eval_extents(inputs: &EvalInputs<'_>, display: [u32; 2]) -> Result<(), DlssError> {
    let size = |view: &wgpu::TextureView| {
        let texture = view.texture();
        [texture.width(), texture.height()]
    };
    let mut inputs_views = vec![inputs.color, inputs.depth, inputs.motion];
    inputs_views.extend(inputs.reactive);
    inputs_views.extend(inputs.transparency);
    let sizes: Vec<[u32; 2]> = inputs_views.into_iter().map(size).collect();
    let exposure = inputs.exposure.map(|view| (size(view), view.texture().format()));
    check_extents(inputs.render_size, display, &sizes, size(inputs.output), exposure)
}

/// The pure part of [`check_eval_extents`], on plain sizes.
fn check_extents(
    render: [u32; 2],
    display: [u32; 2],
    inputs: &[[u32; 2]],
    output: [u32; 2],
    exposure: Option<([u32; 2], wgpu::TextureFormat)>,
) -> Result<(), DlssError> {
    let invalid = || DlssError::Ngx {
        call: "evaluate",
        failure: NgxFailure::InvalidParameter,
    };
    if render.contains(&0) || inputs.iter().any(|s| s[0] < render[0] || s[1] < render[1]) {
        return Err(invalid());
    }
    if output[0] < display[0] || output[1] < display[1] {
        return Err(invalid());
    }
    if let Some((size, format)) = exposure {
        if size != [1, 1] {
            return Err(invalid());
        }
        if format != wgpu::TextureFormat::R32Float {
            return Err(DlssError::UnsupportedFormat(format));
        }
    }
    Ok(())
}

/// The create-time size check: every side of `display` and `render_max` must be at least 1
/// (a zero side reaches NGX as a zero `InWidth`/`InTargetWidth`).
// Only the linked build calls this (it guards `ngx.rs`'s create); tests cover it everywhere.
#[cfg_attr(not(regia_dlss_linked), allow(dead_code))]
pub(crate) fn check_create_sizes(display: [u32; 2], render_max: [u32; 2]) -> Result<(), DlssError> {
    if display.contains(&0) || render_max.contains(&0) {
        return Err(DlssError::Ngx {
            call: "CREATE_DLSS_EXT",
            failure: NgxFailure::InvalidParameter,
        });
    }
    Ok(())
}

/// The per-frame render-size check against the create-time maximum: a zero side or one above
/// `render_max` reaches NGX and corrupts the output.
// Only the linked build calls this (it guards `ngx.rs`'s evaluate); tests cover it everywhere.
#[cfg_attr(not(regia_dlss_linked), allow(dead_code))]
pub(crate) fn check_render_size(render_size: [u32; 2], render_max: [u32; 2]) -> Result<(), DlssError> {
    if render_size
        .iter()
        .zip(render_max)
        .any(|(&size, max)| size == 0 || size > max)
    {
        return Err(DlssError::Ngx {
            call: "EVALUATE_DLSS_EXT",
            failure: NgxFailure::InvalidParameter,
        });
    }
    Ok(())
}

/// A path as UTF-8 for NGX; a path that is not valid UTF-8 is refused instead of being
/// rewritten with replacement characters (NGX would search a directory that does not exist).
// Only the non-Windows linked build calls this (Windows paths are UTF-16); tests cover it everywhere.
#[cfg_attr(not(all(regia_dlss_linked, not(target_os = "windows"))), allow(dead_code))]
pub(crate) fn utf8_path(path: &std::ffi::OsStr) -> Result<&str, DlssError> {
    path.to_str().ok_or(DlssError::Ngx {
        call: "path",
        failure: NgxFailure::InvalidParameter,
    })
}

/// The refusal for a device other than the one NGX (or the feature) lives on: NGX would record
/// one device's parameter block into another device's command buffer.
fn device_mismatch() -> DlssError {
    DlssError::Ngx {
        call: "device_mismatch",
        failure: NgxFailure::InvalidParameter,
    }
}

/// A DLSS Super Resolution feature for one display size, mode and flag set.
pub struct SuperResolution(imp::SuperResolution);

impl std::fmt::Debug for SuperResolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SuperResolution").finish_non_exhaustive()
    }
}

impl SuperResolution {
    /// Creates the feature (submits its creation on `queue`). `render_max` is the largest
    /// render size it will see ([`OptimalSettings::max`]).
    ///
    /// # Errors
    ///
    /// An NGX failure, [`DlssError::NotVulkan`] or [`DlssError::Poisoned`]. A `device` other
    /// than the one `ngx` was initialised on is refused with [`NgxFailure::InvalidParameter`]
    /// (call `device_mismatch`) before NGX is called (audit 11-03, dlss M4). A zero side of
    /// `display` or `render_max` is refused with [`NgxFailure::InvalidParameter`] before NGX is
    /// called (edge audit 1102, plan row A06).
    pub fn new(
        ngx: &Ngx,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        display: [u32; 2],
        quality: PerfQuality,
        flags: CreateFlags,
        render_max: [u32; 2],
    ) -> Result<Self, DlssError> {
        if crate::LINKED && !ngx.is_on(device) {
            return Err(device_mismatch());
        }
        imp::SuperResolution::new(&ngx.0, device, queue, display, quality, flags, render_max).map(Self)
    }

    /// Records the inputs' transitions on `encoder` and returns the DLSS command buffer, which
    /// must be submitted right after `encoder`'s buffer and before any later work of the frame.
    ///
    /// The DLSS buffer holds raw image handles that wgpu does not track; what keeps every input
    /// and the output alive is `encoder`, whose tracker holds each texture it transitioned until
    /// that submission completes. So submit `encoder`'s buffer and the DLSS buffer together
    /// (`[encoder.finish(), dlss]`): dropping or never submitting `encoder` while submitting the
    /// DLSS buffer leaves it pointing at freed images.
    ///
    /// # Errors
    ///
    /// An unsupported format, a parent texture smaller than `render_size` (or an output parent
    /// smaller than the display size, or an exposure that is not 1×1 `R32Float`), a `render_size`
    /// of zero or above the create-time `render_max`, an NGX failure, [`DlssError::NotVulkan`] or
    /// [`DlssError::Poisoned`]. The size check uses the parent texture's base level, so a smaller
    /// mip view of a large texture is not rejected here. A `device` other than the feature's is
    /// refused with [`NgxFailure::InvalidParameter`] (call `device_mismatch`) before NGX is called.
    pub fn evaluate(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &EvalInputs<'_>,
    ) -> Result<DlssCommands, DlssError> {
        if crate::LINKED && !self.0.is_on(device) {
            return Err(device_mismatch());
        }
        check_eval_extents(inputs, self.0.display())?;
        let (buffer, lease) = self.0.evaluate(device, encoder, inputs)?;
        Ok(DlssCommands {
            buffer,
            lease: FeatureLease { _lease: lease },
        })
    }

    /// The display size the feature was created for.
    #[must_use]
    pub fn display(&self) -> [u32; 2] {
        self.0.display()
    }
}

/// The DLSS command buffer of one [`SuperResolution::evaluate`], with a lease on its feature.
///
/// The buffer records the raw NGX feature handle. Before audit 11-03 (dlss H1) it was a plain
/// `wgpu::CommandBuffer`, so safe code could drop the [`SuperResolution`] (which releases the
/// feature after waiting only for work already submitted) and submit the buffer afterwards,
/// running NGX's commands on a released feature. The lease keeps the feature alive until the
/// buffer has been submitted: a feature is released when its [`SuperResolution`] and every lease
/// are gone, after waiting for the GPU.
#[must_use = "the DLSS commands do nothing until they are submitted"]
pub struct DlssCommands {
    buffer: wgpu::CommandBuffer,
    lease: FeatureLease,
}

impl std::fmt::Debug for DlssCommands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DlssCommands").finish_non_exhaustive()
    }
}

impl DlssCommands {
    /// Submits `host` (the buffer of the encoder `evaluate` recorded the transitions on) and then
    /// the DLSS buffer, in one submission, and gives up the lease once both are queued.
    pub fn submit(self, queue: &wgpu::Queue, host: wgpu::CommandBuffer) -> wgpu::SubmissionIndex {
        // `_lease` lives to the end of this function, after the submission.
        let Self {
            buffer,
            lease: _lease,
        } = self;
        queue.submit([host, buffer])
    }

    /// The buffer and its lease, for a host that submits the buffer later together with other
    /// work (the renderer's frame submits every buffer at its end). Keep the lease until the
    /// buffer was submitted (or dropped unsubmitted).
    #[must_use]
    pub fn into_parts(self) -> (wgpu::CommandBuffer, FeatureLease) {
        (self.buffer, self.lease)
    }
}

/// Keeps a DLSS feature (and its NGX instance) from being released while a command buffer that
/// records it may still be submitted (see [`DlssCommands`]). Dropping it is all it does.
pub struct FeatureLease {
    _lease: imp::Lease,
}

impl std::fmt::Debug for FeatureLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeatureLease").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_snake_case_and_map_the_ngx_results() {
        assert_eq!(NgxFailure::from_code(1), None);
        assert_eq!(NgxFailure::from_code(0x2), None);
        assert_eq!(
            NgxFailure::from_code(0xBAD0_0001),
            Some(NgxFailure::FeatureNotSupported)
        );
        assert_eq!(
            NgxFailure::from_code(0xBAD0_000B),
            Some(NgxFailure::UnableToInitializeFeature)
        );
        assert_eq!(NgxFailure::from_code(0xBAD0_000C), Some(NgxFailure::OutOfDate));
        assert_eq!(
            NgxFailure::from_code(0xBAD0_0012),
            Some(NgxFailure::NotImplemented)
        );
        assert_eq!(
            NgxFailure::from_code(0xBAD0_0000),
            Some(NgxFailure::Unknown(0xBAD0_0000))
        );
        // Inside NGX's failure range (top 12 bits 0xBAD) but not a named code: an unknown
        // failure, never a success (audit upscalers M4).
        assert_eq!(
            NgxFailure::from_code(0xBAD0_1000),
            Some(NgxFailure::Unknown(0xBAD0_1000))
        );
        assert_eq!(
            NgxFailure::from_code(0xBADF_0001),
            Some(NgxFailure::Unknown(0xBADF_0001))
        );
        assert_eq!(
            NgxFailure::from_code(0xBAD0_0099),
            Some(NgxFailure::Unknown(0xBAD0_0099))
        );
        let errors = [
            DlssError::NotLinked,
            DlssError::NotVulkan,
            DlssError::Unavailable,
            DlssError::NeedsNewerDriver,
            DlssError::Ngx {
                call: "x",
                failure: NgxFailure::Denied,
            },
            DlssError::Vulkan(String::new()),
            DlssError::Wgpu(String::new()),
            DlssError::UnsupportedFormat(wgpu::TextureFormat::Bgra8Unorm),
            DlssError::Poisoned,
        ];
        let failures = (0..=0x13).filter_map(|c| NgxFailure::from_code(0xBAD0_0000 + c));
        let names = errors
            .iter()
            .map(DlssError::code)
            .chain(failures.map(NgxFailure::name));
        for name in names {
            assert!(!name.is_empty());
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{name}"
            );
        }
    }

    /// Audit upscalers M4 (muse): NGX's own failure test masks the top 12 bits
    /// (`code & 0xFFF0_0000 == 0xBAD0_0000`), so a code of that range with high low bits, or
    /// an extended family such as `0xBAD1_0001`, is a failure (an unknown one), never a success
    /// that lets the caller read a half-filled out-parameter.
    #[test]
    fn every_code_of_the_failure_range_is_a_failure() {
        for code in [0xBAD0_1000, 0xBAD1_0001, 0xBADF_0001, 0xBADF_FFFF] {
            assert_eq!(
                NgxFailure::from_code(code),
                Some(NgxFailure::Unknown(code)),
                "{code:#x}"
            );
        }
        assert_eq!(
            NgxFailure::from_code(0xBAD0_0005),
            Some(NgxFailure::InvalidParameter)
        );
        assert_eq!(NgxFailure::from_code(1), None);
        assert_eq!(NgxFailure::from_code(0xBAE0_0001), None);
    }

    /// Zero create sizes, a zero or above-`render_max` render size, and a non-UTF-8 SDK path
    /// must be refused before they reach NGX (edge audit 1102, plan row A06).
    #[test]
    fn sizes_and_paths_are_validated() {
        assert!(check_create_sizes([1920, 1080], [1280, 720]).is_ok());
        assert!(check_create_sizes([0, 1080], [1280, 720]).is_err());
        assert!(check_create_sizes([1920, 1080], [1280, 0]).is_err());
        assert!(check_render_size([1280, 720], [1280, 720]).is_ok());
        assert!(check_render_size([640, 360], [1280, 720]).is_ok());
        assert!(check_render_size([0, 360], [1280, 720]).is_err());
        assert!(check_render_size([1281, 720], [1280, 720]).is_err());
        assert!(check_render_size([1280, 721], [1280, 720]).is_err());
        assert_eq!(utf8_path(std::ffi::OsStr::new("/tmp/ngx")), Ok("/tmp/ngx"));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(utf8_path(std::ffi::OsStr::from_bytes(b"/tmp/\xff")).is_err());
        }
    }

    #[test]
    fn eval_extents_reject_small_views_and_a_wrong_exposure() {
        use wgpu::TextureFormat::{R8Unorm, R32Float};
        let ok = |inputs: &[[u32; 2]], out, exp| check_extents([1920, 1080], [3840, 2160], inputs, out, exp);
        let big = [[1920, 1080], [2560, 1440]];
        assert_eq!(ok(&big, [3840, 2160], Some(([1, 1], R32Float))), Ok(()));
        assert_eq!(ok(&big, [3840, 2160], None), Ok(()));
        assert!(ok(&[[4, 4]], [3840, 2160], None).is_err());
        assert!(ok(&[[1920, 1079]], [3840, 2160], None).is_err());
        assert!(ok(&big, [3840, 2159], None).is_err());
        assert!(ok(&big, [3840, 2160], Some(([2, 1], R32Float))).is_err());
        assert_eq!(
            ok(&big, [3840, 2160], Some(([1, 1], R8Unorm))),
            Err(DlssError::UnsupportedFormat(R8Unorm))
        );
        assert!(check_extents([0, 0], [8, 8], &[], [8, 8], None).is_err());
    }

    #[test]
    fn the_stub_refuses_without_panicking() {
        if LINKED {
            return;
        }
        assert_eq!(SDK_VERSION, "none");
        let config = NgxConfig::new(Vec::new());
        assert_eq!(config.project_id, PROJECT_ID);
        let descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        assert_eq!(
            create_instance(&descriptor, &config).err(),
            Some(DlssError::NotLinked)
        );
    }

    #[cfg(not(regia_dlss_linked))]
    #[test]
    fn stub_quality_queries_and_vram_refuse_without_panicking() {
        const { assert!(!LINKED) };
        assert_eq!(SDK_VERSION, "none");

        let ngx = imp::Ngx;
        for quality in [
            PerfQuality::Dlaa,
            PerfQuality::Quality,
            PerfQuality::Balanced,
            PerfQuality::Performance,
            PerfQuality::UltraPerformance,
        ] {
            assert_eq!(
                ngx.optimal_settings([1920, 1080], quality),
                Err(DlssError::NotLinked),
                "{quality:?}"
            );
        }
        assert_eq!(ngx.vram_bytes(), Err(DlssError::NotLinked));
        assert_eq!(imp::stub_entry_tests::request_device(), Err(DlssError::NotLinked));
        assert_eq!(imp::stub_entry_tests::ngx_new(), Err(DlssError::NotLinked));
        assert_eq!(
            imp::stub_entry_tests::super_resolution_new(),
            Err(DlssError::NotLinked)
        );
    }
}
