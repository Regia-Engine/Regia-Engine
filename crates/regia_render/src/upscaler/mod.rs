// SPDX-License-Identifier: MPL-2.0
//! One seam for the temporal upscalers (ADR-324).
//!
//! TAAU (`taau.rs`), the TSR-grade upscaler (`tsr/`) and the FSR 3.1 port (`fsr3/`) each had
//! their own call in the frame. They now share one internal trait, `UpscalerBackend`, and
//! one input set, `UpscaleInputs`: colour, depth, motion vectors, exposure, the reactive and
//! transparency masks, jitter, history reset, render size and display size, plus the camera
//! matrices and the TAAU-only knobs. Each backend reads what it uses. The frame picks the
//! backend with `select`, which keeps the pre-seam rule: the profile's [`TemporalUpscaler`]
//! runs on a single-sample TAA frame, and every other frame that reaches the upscale step
//! (MSAA, or a scaled frame without TAA) runs TAAU's resolve.
//!
//! The public half of the module is the part a host or the agent protocol sees:
//!
//! - [`UpscaleQuality`]: the vendor-style quality modes with their per-axis ratios
//!   (1.0, 1.5, 1.7, 2.0, 3.0) and Regia Engine's integer rounding rule
//!   ([`UpscaleQuality::render_size`]).
//! - [`VendorUpscaler`]: DLSS and XeSS, kept out of [`TemporalUpscaler`] so a host's
//!   exhaustive match on it keeps compiling. No vendor SDK is in the tree; the vendor
//!   integration plan is ADR-325.
//! - [`available`]: a pure availability probe that says, per backend, whether it can run on an
//!   adapter and, if not, why ([`reason`]).
//! - [`plugin`]: the public boundary a vendor or software upscaler implements (ADR-510): the
//!   [`plugin::Upscaler`] trait, its input contract and conventions, and one feature probe per
//!   backend.
//! - [`reference`](mod@reference): a software reference upscaler (bilinear or an edge-adaptive spatial filter,
//!   temporal accumulation driven by jitter, motion vectors and the reactive mask) that runs
//!   through that boundary on the CPU.
//! - [`vendor`]: the vendor integration seam (ADR-1150): the DLSS and XeSS stub bindings, the
//!   feature lifecycle around a vendor runtime ([`vendor::VendorBackend`]), runtime library
//!   discovery, the Vulkan extension check and the ratio-scaled jitter sequence
//!   ([`vendor::JitterSequence`]).
//!
//! The quality ratios are the published per-axis scale factors of the vendor upscalers'
//! quality modes (AMD FidelityFX Super Resolution 2 README, "Quality modes"; NVIDIA DLSS
//! programming guide, "Optimal settings"); the rounding rule and the dynamic-resolution ranges
//! are Regia Engine's (`docs/provenance.md`).

pub mod plugin;
pub mod reference;
pub mod vendor;

use regia_core::glam::Mat4;

use crate::RenderError;
use crate::TemporalUpscaler;

/// Upscale quality mode: how much smaller than the display the frame renders, per axis.
///
/// The ratio is display size ÷ render size on each axis: 1.0, 1.5, 1.7, 2.0 and 3.0. A
/// [`crate::RenderProfile::upscale_quality`] of `None` (every preset) keeps the profile's
/// explicit `render_scale`; `Some(mode)` replaces it with the mode's scale
/// ([`Self::permille`]) and renders at exactly [`Self::render_size`] while the frame's permille
/// is that scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpscaleQuality {
    /// Ratio 1.0: render at the display size (the upscaler only anti-aliases).
    Native,
    /// Ratio 1.5 (render scale 0.667).
    Quality,
    /// Ratio 1.7 (render scale 0.588).
    Balanced,
    /// Ratio 2.0 (render scale 0.5).
    Performance,
    /// Ratio 3.0 (render scale 0.333).
    UltraPerformance,
}

impl UpscaleQuality {
    /// Every mode, from the largest render size to the smallest.
    pub const ALL: [Self; 5] = [
        Self::Native,
        Self::Quality,
        Self::Balanced,
        Self::Performance,
        Self::UltraPerformance,
    ];

    /// The mode whose ratio is nearest the frame's larger per-axis display ÷ render ratio (a
    /// profile's own render scale picks the vendor feature's mode, ADR-2002). Zero sizes read
    /// as 1.
    #[must_use]
    pub fn nearest(render: (u32, u32), display: (u32, u32)) -> Self {
        let axis = |r: u32, d: u32| d.max(1) as f32 / r.max(1) as f32;
        let ratio = axis(render.0, display.0).max(axis(render.1, display.1));
        let mut best = Self::Native;
        for mode in Self::ALL {
            if (mode.ratio() - ratio).abs() < (best.ratio() - ratio).abs() {
                best = mode;
            }
        }
        best
    }

    /// The per-axis ratio display ÷ render as an exact fraction `(numerator, denominator)`:
    /// 1/1, 3/2, 17/10, 2/1, 3/1. [`Self::render_size`] rounds with these, not with an `f32`.
    #[must_use]
    pub const fn ratio_fraction(self) -> (u32, u32) {
        match self {
            Self::Native => (1, 1),
            Self::Quality => (3, 2),
            Self::Balanced => (17, 10),
            Self::Performance => (2, 1),
            Self::UltraPerformance => (3, 1),
        }
    }

    /// The per-axis ratio display ÷ render: 1.0, 1.5, 1.7, 2.0 or 3.0.
    #[must_use]
    pub fn ratio(self) -> f32 {
        let (num, den) = self.ratio_fraction();
        num as f32 / den as f32
    }

    /// Stable snake_case name used by the agent protocol: `native`, `quality`, `balanced`,
    /// `performance`, `ultra_performance`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Quality => "quality",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
            Self::UltraPerformance => "ultra_performance",
        }
    }

    /// Parses [`Self::name`]; case, `_`, `-` and spaces are ignored (`UltraPerformance`,
    /// `ultra-performance` and `ultra_performance` all match).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let key = normalized(name);
        Self::ALL.into_iter().find(|mode| normalized(mode.name()) == key)
    }

    /// Render size of one axis for a `display` size: `display × den ÷ num` rounded to the
    /// nearest integer, halves up, at least 1. Computed in integers, so it is exact.
    #[must_use]
    pub const fn render_axis(self, display: u32) -> u32 {
        let (num, den) = self.ratio_fraction();
        let num = num as u64;
        let scaled = (display as u64 * den as u64 * 2 + num) / (2 * num);
        if scaled == 0 { 1 } else { scaled as u32 }
    }

    /// Render size for a `display` size (width, height), per axis by [`Self::render_axis`].
    ///
    /// | Display | Native | Quality | Balanced | Performance | Ultra Performance |
    /// |---|---|---|---|---|---|
    /// | 2560×1440 | 2560×1440 | 1707×960 | 1506×847 | 1280×720 | 853×480 |
    /// | 3840×2160 | 3840×2160 | 2560×1440 | 2259×1271 | 1920×1080 | 1280×720 |
    ///
    /// A vendor backend asks its own runtime for the render size it wants (ADR-325); this rule
    /// is what TAAU, TSR and FSR 3.1 render at.
    #[must_use]
    pub const fn render_size(self, display: (u32, u32)) -> (u32, u32) {
        (self.render_axis(display.0), self.render_axis(display.1))
    }

    /// The mode's render scale in permille (`1000 × den ÷ num`, rounded the same way): 1000,
    /// 667, 588, 500, 333. [`crate::FrameQuality::from_profile`] writes it to
    /// `render_scale_permille`.
    #[must_use]
    pub const fn permille(self) -> u32 {
        self.render_axis(1000)
    }

    /// The next mode down (smaller render size); Ultra Performance is its own.
    #[must_use]
    pub const fn next_lower(self) -> Self {
        match self {
            Self::Native => Self::Quality,
            Self::Quality => Self::Balanced,
            Self::Balanced => Self::Performance,
            Self::Performance | Self::UltraPerformance => Self::UltraPerformance,
        }
    }

    /// The permille range dynamic resolution may move in under this mode, `(min, max)`: from
    /// the mode's own scale down to the next mode's. Native 667–1000, Quality 588–667, Balanced
    /// 500–588, Performance 333–500; Ultra Performance holds 333
    /// ([`crate::DynResConfig::within_quality`]).
    #[must_use]
    pub const fn permille_range(self) -> (u32, u32) {
        (self.next_lower().permille(), self.permille())
    }
}

fn normalized(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

/// A vendor upscaler Regia Engine has a plan for but does not ship (ADR-325).
///
/// Separate from [`TemporalUpscaler`] on purpose: a new variant there would break every host's
/// exhaustive match. This enum is `#[non_exhaustive]`, so a host matching it needs a wildcard
/// arm and later vendors are not a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VendorUpscaler {
    /// NVIDIA DLSS Super Resolution (NGX runtime; NVIDIA RTX GPUs; Vulkan: the binding has no DX12
    /// path yet, ADR-7695).
    Dlss,
    /// Intel XeSS Super Resolution (XeSS runtime; Vulkan or DX12).
    Xess,
}

impl VendorUpscaler {
    /// Every vendor upscaler, in the order [`available`] reports them.
    pub const ALL: [Self; 2] = [Self::Dlss, Self::Xess];

    /// Stable snake_case name: `dlss`, `xess`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dlss => "dlss",
            Self::Xess => "xess",
        }
    }
}

/// Any upscaler backend: one Regia Engine ships ([`TemporalUpscaler`]) or a vendor one
/// ([`VendorUpscaler`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UpscalerKind {
    /// A built-in temporal upscaler (always compiled in).
    Builtin(TemporalUpscaler),
    /// A vendor upscaler (not bundled; see [`available`]).
    Vendor(VendorUpscaler),
}

impl UpscalerKind {
    /// Every kind, in the order [`available`] reports them.
    pub const ALL: [Self; 5] = [
        Self::Builtin(TemporalUpscaler::Taau),
        Self::Builtin(TemporalUpscaler::Tsr),
        Self::Builtin(TemporalUpscaler::Fsr3),
        Self::Vendor(VendorUpscaler::Dlss),
        Self::Vendor(VendorUpscaler::Xess),
    ];

    /// Stable snake_case name: `taau`, `tsr`, `fsr3`, `dlss`, `xess`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Builtin(TemporalUpscaler::Taau) => "taau",
            Self::Builtin(TemporalUpscaler::Tsr) => "tsr",
            Self::Builtin(TemporalUpscaler::Fsr3) => "fsr3",
            Self::Vendor(vendor) => vendor.name(),
        }
    }

    /// Parses [`Self::name`]; case, `_`, `-` and spaces are ignored (`FSR 3`, `fsr-3` and
    /// `fsr3` all match).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let key = normalized(name);
        Self::ALL.into_iter().find(|kind| normalized(kind.name()) == key)
    }
}

/// Stable snake_case reasons an upscaler cannot run, as [`UpscalerStatus::reason`] and the
/// agent protocol report them (`book/agents/upscalers.md`).
pub mod reason {
    /// No vendor SDK runtime is bundled or loaded by this build (every vendor kind today).
    pub const SDK_NOT_BUNDLED: &str = "sdk_not_bundled";
    /// DLSS runs only on NVIDIA adapters (PCI vendor 0x10DE).
    pub const NEEDS_NVIDIA_ADAPTER: &str = "needs_nvidia_adapter";
    /// The vendor runtime needs Vulkan or DX12; the device runs on another backend (GL,
    /// Metal, WebGPU).
    pub const NEEDS_DX12_OR_VULKAN: &str = "needs_dx12_or_vulkan";
    /// On Vulkan the vendor runtime needs instance and device extensions requested at device
    /// creation; this device was created without them (ADR-325, the device-creation hook).
    pub const NEEDS_DX12_OR_VULKAN_EXTENSION: &str = "needs_dx12_or_vulkan_extension";
    /// A software rasterizer (lavapipe, WARP, SwiftShader) cannot run the vendor kernels.
    pub const NEEDS_HARDWARE_ADAPTER: &str = "needs_hardware_adapter";
    /// A mobile GPU (Adreno, Mali, PowerVR, Apple): the vendor runtime targets desktop GPUs.
    pub const NEEDS_DESKTOP_GPU: &str = "needs_desktop_gpu";
    /// There is no render device yet (headless tool, or before the first frame).
    pub const NO_RENDER_DEVICE: &str = "no_render_device";
    /// The vendor integration is built in but its runtime library was not found next to the
    /// executable or on its search path (ADR-325 (7)).
    pub const RUNTIME_NOT_FOUND: &str = "runtime_not_found";
    /// The vendor integration is built and its runtime library was found, but no render
    /// device has initialised it yet (ADR-2002).
    pub const RUNTIME_NOT_INITIALIZED: &str = "runtime_not_initialized";
    /// The vendor runtime loaded and reports the feature unusable on this device.
    pub const RUNTIME_REPORTS_UNSUPPORTED: &str = "runtime_reports_unsupported";
    /// The vendor runtime loaded and asks for a newer GPU driver.
    pub const NEEDS_NEWER_DRIVER: &str = "needs_newer_driver";
    /// The vendor runtime was found but failed to start on this device, for a reason other than
    /// the GPU or the driver: a Vulkan or wgpu failure, or an NGX error during init (ADR-2679).
    pub const RUNTIME_INIT_FAILED: &str = "runtime_init_failed";
    /// The XeSS adapter implements Vulkan only (ADR-4656).
    pub const XESS_NEEDS_VULKAN: &str = "xess_needs_vulkan";
    /// The DLSS binding implements NGX on Vulkan only; the device runs on DX12 (ADR-7695).
    pub const DLSS_NEEDS_VULKAN: &str = "dlss_needs_vulkan";
}

/// PCI vendor ids the probe looks at.
mod vendor_id {
    pub(super) const NVIDIA: u32 = 0x10DE;
    pub(super) const QUALCOMM: u32 = 0x5143;
    pub(super) const ARM: u32 = 0x13B5;
    pub(super) const IMAGINATION: u32 = 0x1010;
    pub(super) const APPLE: u32 = 0x106B;
}

/// What the render device was created with beyond wgpu's own feature set, for [`available`].
///
/// The DLSS device-creation hook (the `dlss` feature, ADR-325, ADR-2002) and the XeSS hook
/// (ADR-4656) enable the extensions each runtime asks for. [`Self::default`] means those
/// extensions were not reported: a probe that has not asked, or a device the hook did not
/// prepare. It is not a claim that every device lacks them. An empty NGX requirement list is
/// prepared: [`vendor::dlss::dlss_vulkan_extensions_flag`] is true for that device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpscalerFeatures {
    /// The Vulkan extensions the DLSS runtime asked for are enabled on this device.
    ///
    /// True when NGX's list for this device is satisfied, including when that list is empty.
    /// False when the probe has not asked ([`Self::default`]) or the device is missing one.
    pub dlss_vulkan_extensions: bool,
    /// The Vulkan instance and device extensions and features the XeSS runtime asked for were
    /// enabled.
    pub xess_vulkan_extensions: bool,
}

/// Whether one upscaler can run on the device, and why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpscalerStatus {
    /// The upscaler.
    pub kind: UpscalerKind,
    /// It can be selected and will run.
    pub available: bool,
    /// The first entry of [`Self::blockers`] (`None` when available): one of the [`reason`]
    /// constants.
    pub reason: Option<&'static str>,
    /// Every reason it cannot run, most fundamental first (hardware, then API, then
    /// extensions, then the SDK). Empty when available.
    pub blockers: Vec<&'static str>,
}

impl UpscalerStatus {
    fn from_blockers(kind: UpscalerKind, blockers: Vec<&'static str>) -> Self {
        Self {
            kind,
            available: blockers.is_empty(),
            reason: blockers.first().copied(),
            blockers,
        }
    }

    /// The one stable code that says what to fix (ADR-2679), `None` when available. A GPU that
    /// can never run the backend comes first, then the runtime's own state (no SDK, library
    /// missing, driver too old, GPU refused, init failed), then the device: a device created
    /// without the vendor's extensions is what a missing SDK looks like on Vulkan, so
    /// [`reason::NEEDS_DX12_OR_VULKAN_EXTENSION`] only wins when nothing else is wrong.
    /// [`Self::reason`] stays the first blocker in probe order.
    #[must_use]
    pub fn unavailable_code(&self) -> Option<&'static str> {
        const HARDWARE: [&str; 3] = [
            reason::NEEDS_NVIDIA_ADAPTER,
            reason::NEEDS_HARDWARE_ADAPTER,
            reason::NEEDS_DESKTOP_GPU,
        ];
        const DEVICE: [&str; 5] = [
            reason::NEEDS_DX12_OR_VULKAN,
            reason::NEEDS_DX12_OR_VULKAN_EXTENSION,
            reason::NO_RENDER_DEVICE,
            reason::XESS_NEEDS_VULKAN,
            reason::DLSS_NEEDS_VULKAN,
        ];
        let first = |wanted: &dyn Fn(&str) -> bool| self.blockers.iter().copied().find(|b| wanted(b));
        first(&|b| HARDWARE.contains(&b))
            .or_else(|| first(&|b| !DEVICE.contains(&b)))
            .or_else(|| self.blockers.first().copied())
    }
}

/// Availability of every upscaler on an adapter: TAAU, TSR, FSR 3.1, DLSS, XeSS, in that order.
///
/// Pure: it reads only its arguments. `backend` is the backend the device runs on (normally
/// `adapter_info.backend`); `features` is what the caller measured for that device
/// ([`UpscalerFeatures::default`] means the vendor extensions were not reported). TAAU, TSR and FSR 3.1 run wherever
/// Regia Engine compiles, so they are always available. DLSS and XeSS list every blocker:
///
/// - DLSS: [`reason::NEEDS_NVIDIA_ADAPTER`], [`reason::DLSS_NEEDS_VULKAN`] (DX12),
///   [`reason::NEEDS_DX12_OR_VULKAN`] (GL, Metal, WebGPU),
///   [`reason::NEEDS_DX12_OR_VULKAN_EXTENSION`] (Vulkan without the extensions),
///   [`reason::SDK_NOT_BUNDLED`].
/// - XeSS: [`reason::NEEDS_HARDWARE_ADAPTER`], [`reason::NEEDS_DESKTOP_GPU`],
///   [`reason::XESS_NEEDS_VULKAN`], [`reason::NEEDS_DX12_OR_VULKAN_EXTENSION`],
///   [`reason::SDK_NOT_BUNDLED`].
///
/// Each backend's rules live in its probe ([`plugin::DlssProbe`], [`plugin::XessProbe`],
/// [`plugin::BuiltinProbe`]); this is [`plugin::available_with`] with
/// [`plugin::VendorRuntimes::current`] (the process's vendor runtimes, ADR-510, ADR-2002).
#[must_use]
pub fn available(
    adapter_info: &wgpu::AdapterInfo,
    backend: wgpu::Backend,
    features: &UpscalerFeatures,
) -> Vec<UpscalerStatus> {
    let device = plugin::ProbeDevice {
        adapter: adapter_info,
        backend,
        features: *features,
    };
    plugin::available_with(Some(&device), &plugin::VendorRuntimes::current())
}

/// [`available`] without a render device: the built-in upscalers are available (they are
/// compiled in) and every vendor kind is blocked by [`reason::NO_RENDER_DEVICE`] and
/// [`reason::SDK_NOT_BUNDLED`].
#[must_use]
pub fn available_without_device() -> Vec<UpscalerStatus> {
    plugin::available_with(None, &plugin::VendorRuntimes::current())
}

/// The camera of one upscaled frame (TSR and FSR 3.1 reproject with it; TAAU does not).
#[derive(Clone, Copy)]
pub(crate) struct UpscaleCamera {
    /// Camera position (world), to keep the reprojection eye-relative.
    pub(crate) eye: [f32; 3],
    /// Jittered view-projection the colour pass used.
    pub(crate) view_proj: Mat4,
    /// The same without jitter.
    pub(crate) unjittered_view_proj: Mat4,
    /// The previous view-projection the velocity pass used.
    pub(crate) velocity_prev_view_proj: Mat4,
}

/// One frame's inputs to whichever backend upscales it: the union of what TAAU, TSR and
/// FSR 3.1 read today. Each backend reads what it uses.
pub(crate) struct UpscaleInputs<'a> {
    /// Scene-referred HDR colour at the render size (jittered on a TAA frame).
    pub(crate) color: &'a wgpu::TextureView,
    /// Single-sample `Depth32Float` scene depth at the render size; `None` on an MSAA frame
    /// or a scaled frame without TAA (TAAU then uses each pixel's own motion).
    pub(crate) depth: Option<&'a wgpu::TextureView>,
    /// Object motion (`previous uv − current uv`, alpha 1 where written), if recorded. TAAU
    /// reads its own velocity target, which is this view.
    pub(crate) motion: Option<&'a wgpu::TextureView>,
    /// Exposure multiplier (linear HDR → display-referred) for a tonemapped-domain resolve.
    pub(crate) exposure: f32,
    /// Reactive mask (red channel, 0–1) at the render size: VFX, decals, alpha.
    pub(crate) reactive: Option<&'a wgpu::TextureView>,
    /// Transparency-and-composition mask (red channel, 0–1) at the render size. No built-in
    /// backend has a separate one: TSR and FSR 3.1 fold it into the reactive mask (ADR-113 (4)).
    pub(crate) transparency: Option<&'a wgpu::TextureView>,
    /// This frame's projection jitter in NDC (`taau::halton_jitter`).
    pub(crate) jitter: [f32; 2],
    /// Drop the history (camera cut, first frame, size change, or a non-temporal upsample).
    pub(crate) reset: bool,
    /// Render (input) size.
    pub(crate) render_size: (u32, u32),
    /// Display (output) size.
    pub(crate) display_size: (u32, u32),
    /// Camera matrices for reprojection.
    pub(crate) camera: UpscaleCamera,
    /// Scale applied to the history this frame: the HDR target's pre-exposure changed by this
    /// factor since the history was written (0 reads as 1).
    pub(crate) history_scale: f32,
    /// TAAU: history weight of a held view (0 for a non-temporal upsample).
    pub(crate) history_weight: f32,
    /// TAAU: motion-adaptive sharpen strength at full motion (0 = off).
    pub(crate) sharpen: f32,
    /// Other views the frame hands as [`Self::color`] on other frames without reallocating them
    /// (`crate::tsr::TsrInputs::alternates`, ADR-8122).
    pub(crate) alternates: [Option<&'a wgpu::TextureView>; 2],
}

impl<'a> UpscaleInputs<'a> {
    /// The TSR / FSR 3.1 input struct (both take `TsrInputs`). `depth` is the single-sample
    /// scene depth those two require.
    fn tsr_inputs(&self, depth: &'a wgpu::TextureView) -> crate::tsr::TsrInputs<'a> {
        crate::tsr::TsrInputs {
            current: self.color,
            depth,
            velocity: self.motion,
            reactive: self.reactive.or(self.transparency),
            render_size: self.render_size,
            output_size: self.display_size,
            jitter: self.jitter,
            eye: self.camera.eye,
            view_proj: self.camera.view_proj,
            unjittered_view_proj: self.camera.unjittered_view_proj,
            velocity_prev_view_proj: self.camera.velocity_prev_view_proj,
            exposure: self.exposure,
            reset: self.reset,
            alternates: self.alternates,
        }
    }
}

/// The device-side resources a backend records with.
pub(crate) struct UpscaleContext<'a> {
    /// The device (the frame's tracked device derefs to it).
    pub(crate) device: &'a wgpu::Device,
    /// Device-wide pipeline registry (TSR builds its pipelines once per device there).
    pub(crate) shared: &'a crate::device_pipelines::DevicePipelines,
    /// The frame's staging belt for uniform uploads.
    pub(crate) belt: &'a mut crate::staging::StagingBelt,
}

/// A temporal upscaler the frame can dispatch to.
pub(crate) trait UpscalerBackend {
    /// Which upscaler this is.
    fn kind(&self) -> TemporalUpscaler;

    /// Records the upscale of `inputs.color` into an output-size HDR view and returns it. The
    /// view stays valid until the next call at a different size.
    fn upscale(
        &mut self,
        ctx: &mut UpscaleContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &UpscaleInputs<'_>,
    ) -> Result<wgpu::TextureView, RenderError>;
}

impl UpscalerBackend for crate::taau::TaauPass {
    fn kind(&self) -> TemporalUpscaler {
        TemporalUpscaler::Taau
    }

    fn upscale(
        &mut self,
        ctx: &mut UpscaleContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &UpscaleInputs<'_>,
    ) -> Result<wgpu::TextureView, RenderError> {
        let _span = tracing::info_span!("taau").entered();
        let _crumb = crate::breadcrumbs::frame_pass("taau");
        self.set_history_scale(inputs.history_scale);
        self.set_reactive(inputs.reactive);
        self.resolve(
            ctx.device,
            ctx.belt,
            encoder,
            inputs.color,
            inputs.depth,
            inputs.render_size.0,
            inputs.render_size.1,
            inputs.display_size.0,
            inputs.display_size.1,
            inputs.history_weight,
            !inputs.reset,
            inputs.sharpen,
        )
    }
}

impl UpscalerBackend for crate::tsr::TsrPass {
    fn kind(&self) -> TemporalUpscaler {
        TemporalUpscaler::Tsr
    }

    fn upscale(
        &mut self,
        ctx: &mut UpscaleContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &UpscaleInputs<'_>,
    ) -> Result<wgpu::TextureView, RenderError> {
        let Some(depth) = inputs.depth else {
            return Err(RenderError::Shader(
                "tsr needs the single-sample scene depth".into(),
            ));
        };
        self.set_history_scale(inputs.history_scale);
        self.resolve(
            ctx.device,
            ctx.shared,
            ctx.belt,
            encoder,
            &inputs.tsr_inputs(depth),
        )
    }
}

impl UpscalerBackend for crate::fsr3::Fsr3Pass {
    fn kind(&self) -> TemporalUpscaler {
        TemporalUpscaler::Fsr3
    }

    fn upscale(
        &mut self,
        ctx: &mut UpscaleContext<'_>,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &UpscaleInputs<'_>,
    ) -> Result<wgpu::TextureView, RenderError> {
        let Some(depth) = inputs.depth else {
            return Err(RenderError::Shader(
                "fsr3 needs the single-sample scene depth".into(),
            ));
        };
        self.set_history_scale(inputs.history_scale);
        self.resolve(ctx.device, ctx.belt, encoder, &inputs.tsr_inputs(depth))
    }
}

/// The backend that upscales a frame: the `selected` upscaler on a single-sample (`samples`)
/// temporal frame (`temporal`), TAAU's resolve on every other frame that reaches the upscale
/// step (MSAA, or a scaled frame without TAA, which TAAU upsamples without history).
#[must_use]
pub(crate) fn resolve_backend(selected: TemporalUpscaler, temporal: bool, samples: u32) -> TemporalUpscaler {
    if temporal && samples == 1 {
        selected
    } else {
        TemporalUpscaler::Taau
    }
}

/// The pass for `kind` among the three a view owns.
pub(crate) fn select<'a>(
    kind: TemporalUpscaler,
    taau: &'a mut crate::taau::TaauPass,
    tsr: &'a mut crate::tsr::TsrPass,
    fsr3: &'a mut crate::fsr3::Fsr3Pass,
) -> &'a mut dyn UpscalerBackend {
    let backend: &'a mut dyn UpscalerBackend = match kind {
        TemporalUpscaler::Taau => taau,
        TemporalUpscaler::Tsr => tsr,
        TemporalUpscaler::Fsr3 => fsr3,
    };
    debug_assert_eq!(backend.kind(), kind, "select returned the wrong pass");
    backend
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_BUILTIN: [TemporalUpscaler; 3] = [
        TemporalUpscaler::Taau,
        TemporalUpscaler::Tsr,
        TemporalUpscaler::Fsr3,
    ];

    /// The pre-seam dispatch in `frame.rs`, transcribed: `tsr_on` / `fsr3_on`, else TAAU.
    fn pre_seam_dispatch(selected: TemporalUpscaler, taau_on: bool, samples: u32) -> TemporalUpscaler {
        let tsr_on = taau_on && samples == 1 && selected == TemporalUpscaler::Tsr;
        let fsr3_on = taau_on && samples == 1 && selected == TemporalUpscaler::Fsr3;
        if tsr_on || fsr3_on {
            if fsr3_on {
                TemporalUpscaler::Fsr3
            } else {
                TemporalUpscaler::Tsr
            }
        } else {
            TemporalUpscaler::Taau
        }
    }

    #[test]
    fn the_trait_dispatch_picks_the_pre_seam_backend_for_every_upscaler() {
        let mut taau = crate::taau::TaauPass::default();
        let mut tsr = crate::tsr::TsrPass::default();
        let mut fsr3 = crate::fsr3::Fsr3Pass::default();
        for selected in ALL_BUILTIN {
            for taau_on in [false, true] {
                for samples in [1, 4] {
                    let expected = pre_seam_dispatch(selected, taau_on, samples);
                    let kind = resolve_backend(selected, taau_on, samples);
                    assert_eq!(kind, expected, "{selected:?} taau_on={taau_on} samples={samples}");
                    let backend = select(kind, &mut taau, &mut tsr, &mut fsr3);
                    assert_eq!(
                        backend.kind(),
                        expected,
                        "{selected:?} taau_on={taau_on} samples={samples}"
                    );
                }
            }
        }
        // The single-sample TAA frame is the only one that honours the selection.
        assert_eq!(
            resolve_backend(TemporalUpscaler::Fsr3, true, 1),
            TemporalUpscaler::Fsr3
        );
        assert_eq!(
            resolve_backend(TemporalUpscaler::Tsr, true, 4),
            TemporalUpscaler::Taau
        );
        assert_eq!(
            resolve_backend(TemporalUpscaler::Tsr, false, 1),
            TemporalUpscaler::Taau
        );
    }

    #[test]
    fn quality_ratios_are_the_published_ones() {
        let ratios: Vec<u32> = UpscaleQuality::ALL
            .iter()
            .map(|mode| mode.ratio().to_bits())
            .collect();
        let expected: Vec<u32> = [1.0f32, 1.5, 1.7, 2.0, 3.0].iter().map(|r| r.to_bits()).collect();
        assert_eq!(ratios, expected);
        let permille: Vec<u32> = UpscaleQuality::ALL.iter().map(|mode| mode.permille()).collect();
        assert_eq!(permille, vec![1000, 667, 588, 500, 333]);
    }

    #[test]
    fn render_sizes_at_1440p_and_4k_are_exact_and_round_half_up() {
        let at = |display: (u32, u32)| -> Vec<(u32, u32)> {
            UpscaleQuality::ALL
                .iter()
                .map(|mode| mode.render_size(display))
                .collect()
        };
        // 2560 / 1.5 = 1706.67 → 1707; 2560 / 1.7 = 1505.88 → 1506; 1440 / 1.7 = 847.06 → 847;
        // 2560 / 3 = 853.33 → 853.
        assert_eq!(
            at((2560, 1440)),
            vec![(2560, 1440), (1707, 960), (1506, 847), (1280, 720), (853, 480)]
        );
        // 3840 / 1.7 = 2258.82 → 2259; 2160 / 1.7 = 1270.59 → 1271.
        assert_eq!(
            at((3840, 2160)),
            vec![
                (3840, 2160),
                (2560, 1440),
                (2259, 1271),
                (1920, 1080),
                (1280, 720)
            ]
        );
        // A half rounds up: 3 / 2 = 1.5 → 2 (Performance), 9 / 1.5 = 6 exactly.
        assert_eq!(UpscaleQuality::Performance.render_axis(3), 2);
        assert_eq!(UpscaleQuality::Quality.render_axis(9), 6);
        // Never zero.
        assert_eq!(UpscaleQuality::UltraPerformance.render_size((0, 1)), (1, 1));
        // No overflow at the largest axis.
        assert_eq!(UpscaleQuality::Native.render_axis(u32::MAX), u32::MAX);
    }

    #[test]
    fn dynamic_resolution_ranges_span_down_to_the_next_mode() {
        let ranges: Vec<(u32, u32)> = UpscaleQuality::ALL
            .iter()
            .map(|mode| mode.permille_range())
            .collect();
        assert_eq!(
            ranges,
            vec![(667, 1000), (588, 667), (500, 588), (333, 500), (333, 333)]
        );
    }

    #[test]
    fn names_round_trip_and_ignore_separators() {
        for mode in UpscaleQuality::ALL {
            assert_eq!(UpscaleQuality::from_name(mode.name()), Some(mode));
        }
        assert_eq!(
            UpscaleQuality::from_name("UltraPerformance"),
            Some(UpscaleQuality::UltraPerformance)
        );
        assert_eq!(
            UpscaleQuality::from_name("ultra-performance"),
            Some(UpscaleQuality::UltraPerformance)
        );
        assert_eq!(UpscaleQuality::from_name("max"), None);
        for kind in UpscalerKind::ALL {
            assert_eq!(UpscalerKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(
            UpscalerKind::from_name("FSR 3"),
            Some(UpscalerKind::Builtin(TemporalUpscaler::Fsr3))
        );
        assert_eq!(
            UpscalerKind::from_name("DLSS"),
            Some(UpscalerKind::Vendor(VendorUpscaler::Dlss))
        );
        assert_eq!(UpscalerKind::from_name("metalfx"), None);
        // Agent error codes and reasons are lowercase ascii, digits and `_`.
        for code in [
            reason::SDK_NOT_BUNDLED,
            reason::NEEDS_NVIDIA_ADAPTER,
            reason::NEEDS_DX12_OR_VULKAN,
            reason::NEEDS_DX12_OR_VULKAN_EXTENSION,
            reason::NEEDS_HARDWARE_ADAPTER,
            reason::NEEDS_DESKTOP_GPU,
            reason::NO_RENDER_DEVICE,
            reason::RUNTIME_INIT_FAILED,
            reason::XESS_NEEDS_VULKAN,
            reason::DLSS_NEEDS_VULKAN,
        ]
        .into_iter()
        .chain(UpscalerKind::ALL.iter().map(|kind| kind.name()))
        .chain(UpscaleQuality::ALL.iter().map(|mode| mode.name()))
        {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }

    fn adapter(
        vendor: u32,
        device_type: wgpu::DeviceType,
        backend: wgpu::Backend,
        name: &str,
    ) -> wgpu::AdapterInfo {
        let mut info = wgpu::AdapterInfo::new(device_type, backend);
        info.vendor = vendor;
        info.name = name.to_string();
        info
    }

    /// The blocker this process's runtime adds to a vendor row (ADR-4907). An SDK-free build
    /// must report `sdk_not_bundled`; an SDK-linked build reports its runtime's own state
    /// (`runtime_not_found` without the library, nothing once ready), never `sdk_not_bundled`.
    fn runtime_tail(vendor: VendorUpscaler) -> Vec<&'static str> {
        let runtime = plugin::VendorRuntimes::current().get(vendor);
        let linked = match vendor {
            VendorUpscaler::Dlss => vendor::dlss::dlss_status().binding_built,
            VendorUpscaler::Xess => vendor::xess::BINDING_BUILT,
        };
        if linked {
            assert_ne!(runtime, plugin::VendorRuntime::NotBundled, "{vendor:?} is linked");
        } else {
            assert_eq!(
                runtime,
                plugin::VendorRuntime::NotBundled,
                "{vendor:?} is not linked"
            );
        }
        runtime.blocker().into_iter().collect()
    }

    /// `hardware` followed by this process's runtime blocker for `vendor`.
    fn with_runtime(vendor: VendorUpscaler, hardware: &[&'static str]) -> Vec<&'static str> {
        hardware.iter().copied().chain(runtime_tail(vendor)).collect()
    }

    fn status(list: &[UpscalerStatus], name: &str) -> UpscalerStatus {
        list.iter()
            .find(|s| s.kind.name() == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} status")) // invariant: test-only; every kind is listed
    }

    fn builtins_available(list: &[UpscalerStatus]) {
        assert_eq!(list.len(), 5);
        let names: Vec<&str> = list.iter().map(|s| s.kind.name()).collect();
        assert_eq!(names, vec!["taau", "tsr", "fsr3", "dlss", "xess"]);
        for name in ["taau", "tsr", "fsr3"] {
            let s = status(list, name);
            assert!(
                s.available && s.reason.is_none() && s.blockers.is_empty(),
                "{s:?}"
            );
        }
    }

    #[test]
    fn nvidia_vulkan_and_dx12_are_blocked_only_by_what_is_missing() {
        let none = UpscalerFeatures::default();
        let info = adapter(
            0x10DE,
            wgpu::DeviceType::DiscreteGpu,
            wgpu::Backend::Vulkan,
            "NVIDIA GeForce RTX 5090",
        );
        let list = available(&info, wgpu::Backend::Vulkan, &none);
        builtins_available(&list);
        let dlss = status(&list, "dlss");
        assert!(!dlss.available);
        assert_eq!(dlss.reason, Some(reason::NEEDS_DX12_OR_VULKAN_EXTENSION));
        assert_eq!(
            dlss.blockers,
            with_runtime(VendorUpscaler::Dlss, &[reason::NEEDS_DX12_OR_VULKAN_EXTENSION])
        );
        // With the device-creation hook's extensions, only the SDK is missing.
        let hooked = UpscalerFeatures {
            dlss_vulkan_extensions: true,
            xess_vulkan_extensions: true,
        };
        let list = available(&info, wgpu::Backend::Vulkan, &hooked);
        let dlss = status(&list, "dlss");
        assert_eq!(dlss.reason, runtime_tail(VendorUpscaler::Dlss).first().copied());
        assert_eq!(dlss.blockers, runtime_tail(VendorUpscaler::Dlss));
        // DX12 needs no extension hook, but the DLSS binding has no DX12 path (ADR-7695): the row
        // must say so rather than claim only the runtime is missing.
        let dx12 = adapter(
            0x10DE,
            wgpu::DeviceType::DiscreteGpu,
            wgpu::Backend::Dx12,
            "NVIDIA GeForce RTX 5080",
        );
        let list = available(&dx12, wgpu::Backend::Dx12, &none);
        let dlss = status(&list, "dlss");
        assert!(!dlss.available);
        assert_eq!(
            dlss.blockers,
            with_runtime(VendorUpscaler::Dlss, &[reason::DLSS_NEEDS_VULKAN])
        );
        assert_eq!(dlss.reason, Some(reason::DLSS_NEEDS_VULKAN));
        // XeSS is not vendor-locked, but this adapter implements Vulkan only.
        assert_eq!(
            status(&list, "xess").blockers,
            with_runtime(VendorUpscaler::Xess, &[reason::XESS_NEEDS_VULKAN])
        );
        // GL on NVIDIA: the API is wrong, and there is no extension question.
        let gl = available(&info, wgpu::Backend::Gl, &none);
        assert_eq!(
            status(&gl, "dlss").blockers,
            with_runtime(VendorUpscaler::Dlss, &[reason::NEEDS_DX12_OR_VULKAN])
        );
    }

    /// NGX wanted nothing on this device: the recorded hook list stays empty and the extension
    /// blocker must not fire. `Default` (extensions not reported) still carries it.
    #[test]
    fn an_empty_ngx_requirement_list_is_not_a_missing_extension() {
        use super::vendor::dlss::dlss_vulkan_extensions_flag;
        let info = adapter(
            0x10DE,
            wgpu::DeviceType::DiscreteGpu,
            wgpu::Backend::Vulkan,
            "NVIDIA GeForce RTX 5090",
        );
        let ready = UpscalerFeatures {
            dlss_vulkan_extensions: dlss_vulkan_extensions_flag(Some(true), false),
            xess_vulkan_extensions: false,
        };
        let dlss = status(&available(&info, wgpu::Backend::Vulkan, &ready), "dlss");
        assert!(
            !dlss.blockers.contains(&reason::NEEDS_DX12_OR_VULKAN_EXTENSION),
            "{dlss:?}"
        );
        let missing = UpscalerFeatures {
            dlss_vulkan_extensions: dlss_vulkan_extensions_flag(Some(false), false),
            xess_vulkan_extensions: false,
        };
        let blocked = status(&available(&info, wgpu::Backend::Vulkan, &missing), "dlss");
        assert!(
            blocked.blockers.contains(&reason::NEEDS_DX12_OR_VULKAN_EXTENSION),
            "{blocked:?}"
        );
    }

    #[test]
    fn amd_and_intel_cannot_run_dlss() {
        let none = UpscalerFeatures::default();
        for (vendor, name) in [
            (0x1002, "AMD Radeon RX 7900 XTX"),
            (0x8086, "Intel(R) Arc(TM) A770"),
        ] {
            let info = adapter(vendor, wgpu::DeviceType::DiscreteGpu, wgpu::Backend::Vulkan, name);
            let list = available(&info, wgpu::Backend::Vulkan, &none);
            builtins_available(&list);
            let dlss = status(&list, "dlss");
            assert!(!dlss.available);
            assert_eq!(dlss.reason, Some(reason::NEEDS_NVIDIA_ADAPTER), "{name}");
            assert_eq!(
                dlss.blockers,
                with_runtime(
                    VendorUpscaler::Dlss,
                    &[
                        reason::NEEDS_NVIDIA_ADAPTER,
                        reason::NEEDS_DX12_OR_VULKAN_EXTENSION
                    ]
                )
            );
            let xess = status(&list, "xess");
            assert!(!xess.available);
            assert_eq!(
                xess.reason,
                Some(reason::NEEDS_DX12_OR_VULKAN_EXTENSION),
                "{name}"
            );
        }
        let intel_dx12 = adapter(
            0x8086,
            wgpu::DeviceType::IntegratedGpu,
            wgpu::Backend::Dx12,
            "Intel(R) Iris(R) Xe Graphics",
        );
        let list = available(&intel_dx12, wgpu::Backend::Dx12, &none);
        assert_eq!(
            status(&list, "xess").blockers,
            with_runtime(VendorUpscaler::Xess, &[reason::XESS_NEEDS_VULKAN])
        );
    }

    #[test]
    fn lavapipe_is_a_software_adapter() {
        let none = UpscalerFeatures::default();
        let info = adapter(
            0x10005,
            wgpu::DeviceType::Cpu,
            wgpu::Backend::Vulkan,
            "llvmpipe (LLVM 23.1.1, 256 bits)",
        );
        let list = available(&info, wgpu::Backend::Vulkan, &none);
        builtins_available(&list);
        assert_eq!(status(&list, "dlss").reason, Some(reason::NEEDS_NVIDIA_ADAPTER));
        let xess = status(&list, "xess");
        assert_eq!(xess.reason, Some(reason::NEEDS_HARDWARE_ADAPTER));
        assert_eq!(
            xess.blockers,
            with_runtime(
                VendorUpscaler::Xess,
                &[
                    reason::NEEDS_HARDWARE_ADAPTER,
                    reason::NEEDS_DX12_OR_VULKAN_EXTENSION
                ]
            )
        );
    }

    #[test]
    fn android_gpus_get_the_built_ins_only() {
        let none = UpscalerFeatures::default();
        for (vendor, name, backend) in [
            (0x5143, "Adreno (TM) 750", wgpu::Backend::Vulkan),
            (0x13B5, "Mali-G715", wgpu::Backend::Vulkan),
            (0x5143, "Adreno (TM) 610", wgpu::Backend::Gl),
        ] {
            let info = adapter(vendor, wgpu::DeviceType::IntegratedGpu, backend, name);
            let list = available(&info, backend, &none);
            builtins_available(&list);
            let dlss = status(&list, "dlss");
            let xess = status(&list, "xess");
            assert!(!dlss.available && !xess.available, "{name}");
            assert_eq!(dlss.reason, Some(reason::NEEDS_NVIDIA_ADAPTER), "{name}");
            assert_eq!(xess.reason, Some(reason::NEEDS_DESKTOP_GPU), "{name}");
            assert!(
                xess.blockers.ends_with(&runtime_tail(VendorUpscaler::Xess)),
                "{name}"
            );
        }
    }

    #[test]
    fn without_a_device_vendor_kinds_say_so() {
        let list = available_without_device();
        builtins_available(&list);
        for (name, vendor) in [("dlss", VendorUpscaler::Dlss), ("xess", VendorUpscaler::Xess)] {
            let s = status(&list, name);
            assert!(!s.available);
            assert_eq!(s.blockers, with_runtime(vendor, &[reason::NO_RENDER_DEVICE]));
        }
    }

    #[test]
    fn the_probe_is_pure() {
        let info = adapter(
            0x10DE,
            wgpu::DeviceType::DiscreteGpu,
            wgpu::Backend::Vulkan,
            "NVIDIA GeForce RTX 5080",
        );
        let none = UpscalerFeatures::default();
        assert_eq!(
            available(&info, wgpu::Backend::Vulkan, &none),
            available(&info, wgpu::Backend::Vulkan, &none)
        );
    }
}
