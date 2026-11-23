// SPDX-License-Identifier: MPL-2.0
//! The public upscaler boundary (ADR-510): the trait a vendor upscaler (DLSS, XeSS, an AMD
//! runtime FSR) or the software reference implements, the input contract it declares, the
//! conversions from Regia Engine's input conventions into its own, and one feature probe per
//! backend.
//!
//! The frame always hands a backend Regia Engine's own inputs ([`InputConventions::REGIA`]):
//! motion vectors in UV units pointing to the previous frame (`previous uv − current uv`),
//! jitter in NDC (added to the clip position), standard depth (near 0, far 1). A backend whose runtime wants
//! other units converts with [`InputConventions::motion_scale_from_regia`] and
//! [`InputConventions::jitter_from_regia`]; the conversions are exact linear maps, so a
//! runtime that takes a motion-vector scale gets one instead of a converted texture.
//!
//! [`Upscaler`] is generic over where the images live ([`UpscaleTarget`]): [`GpuTarget`] for
//! wgpu texture views (what the vendor crate of ADR-325 implements) and [`CpuTarget`] for the
//! software reference ([`super::reference`]), which runs the same contract on the CPU so its
//! behaviour can be tested without a GPU.

use crate::TemporalUpscaler;

use super::{
    UpscaleQuality, UpscalerFeatures, UpscalerKind, UpscalerStatus, VendorUpscaler, reason, vendor_id,
};

/// Whether a backend reads an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputNeed {
    /// The backend refuses a frame without it ([`UpscaleError::MissingInput`]).
    Required,
    /// Used when given; the backend runs without it.
    Optional,
    /// Ignored.
    Unused,
}

impl InputNeed {
    /// Stable snake_case name: `required`, `optional`, `unused`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Optional => "optional",
            Self::Unused => "unused",
        }
    }
}

/// Units of a motion vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionUnits {
    /// Fractions of the image (0–1 across), y down.
    Uv,
    /// Pixels of the render (input) size, y down.
    RenderPixels,
}

/// Which way a motion vector points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionDirection {
    /// From the pixel's position in this frame to where the surface was in the previous one
    /// (`previous − current`).
    ToPrevious,
    /// From the previous position to this one (`current − previous`).
    ToCurrent,
}

/// Units of the projection jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitterUnits {
    /// Normalized device coordinates, y up (an offset of 2 ÷ width is one pixel).
    Ndc,
    /// Pixels of the render size, y down.
    RenderPixels,
}

/// Sign of the jitter a backend is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitterSign {
    /// The offset the projection was moved by (the scene moves by it on screen).
    Projection,
    /// Its negation: where each pixel centre sampled the unjittered scene.
    Sample,
}

/// How a backend wants motion vectors and jitter expressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputConventions {
    /// Motion-vector units.
    pub motion_units: MotionUnits,
    /// Motion-vector direction.
    pub motion_direction: MotionDirection,
    /// Jitter units.
    pub jitter_units: JitterUnits,
    /// Jitter sign.
    pub jitter_sign: JitterSign,
}

impl InputConventions {
    /// What Regia Engine's frame produces: UV motion to the previous frame (the velocity target),
    /// NDC jitter as added to the projection (`taau::halton_jitter`, `jitter_projection`).
    pub const REGIA: Self = Self {
        motion_units: MotionUnits::Uv,
        motion_direction: MotionDirection::ToPrevious,
        jitter_units: JitterUnits::Ndc,
        jitter_sign: JitterSign::Projection,
    };

    /// Render-pixel motion to the previous frame and render-pixel jitter as applied to the
    /// projection: the convention the public DLSS, XeSS and FSR 2/3 integration guides describe
    /// (each takes a motion-vector scale and pixel jitter; the sign of each vendor's jitter is
    /// to be confirmed against its SDK, ADR-510 (3)). The software reference uses it so its
    /// tests cover the conversion.
    pub const RENDER_PIXELS: Self = Self {
        motion_units: MotionUnits::RenderPixels,
        motion_direction: MotionDirection::ToPrevious,
        jitter_units: JitterUnits::RenderPixels,
        jitter_sign: JitterSign::Projection,
    };

    /// Per-axis factors `s` such that `this convention = s × Regia Engine's` for a motion vector at
    /// `render_size`. A runtime that takes a motion-vector scale is given this; the texture
    /// stays Regia Engine's.
    #[must_use]
    pub fn motion_scale_from_regia(&self, render_size: (u32, u32)) -> [f32; 2] {
        let (sx, sy) = match self.motion_units {
            MotionUnits::Uv => (1.0, 1.0),
            MotionUnits::RenderPixels => (render_size.0 as f32, render_size.1 as f32),
        };
        let sign = match self.motion_direction {
            MotionDirection::ToPrevious => 1.0,
            MotionDirection::ToCurrent => -1.0,
        };
        [sx * sign, sy * sign]
    }

    /// Converts one Regia Engine motion vector into this convention.
    #[must_use]
    pub fn motion_from_regia(&self, motion: [f32; 2], render_size: (u32, u32)) -> [f32; 2] {
        let s = self.motion_scale_from_regia(render_size);
        [motion[0] * s[0], motion[1] * s[1]]
    }

    /// Converts one motion vector in this convention back to Regia Engine's (a zero render size
    /// axis gives 0 on that axis).
    #[must_use]
    pub fn motion_to_regia(&self, motion: [f32; 2], render_size: (u32, u32)) -> [f32; 2] {
        let s = self.motion_scale_from_regia(render_size);
        [safe_div(motion[0], s[0]), safe_div(motion[1], s[1])]
    }

    /// Converts Regia Engine's NDC projection jitter into this convention at `render_size`. One
    /// render pixel is 2 ÷ width in NDC; NDC y is up and pixel y is down.
    #[must_use]
    pub fn jitter_from_regia(&self, jitter_ndc: [f32; 2], render_size: (u32, u32)) -> [f32; 2] {
        let s = self.jitter_scale_from_regia(render_size);
        [jitter_ndc[0] * s[0], jitter_ndc[1] * s[1]]
    }

    /// Converts a jitter in this convention back to Regia Engine's NDC projection jitter.
    #[must_use]
    pub fn jitter_to_regia(&self, jitter: [f32; 2], render_size: (u32, u32)) -> [f32; 2] {
        let s = self.jitter_scale_from_regia(render_size);
        [safe_div(jitter[0], s[0]), safe_div(jitter[1], s[1])]
    }

    fn jitter_scale_from_regia(&self, render_size: (u32, u32)) -> [f32; 2] {
        let (sx, sy) = match self.jitter_units {
            JitterUnits::Ndc => (1.0, 1.0),
            JitterUnits::RenderPixels => (render_size.0 as f32 * 0.5, -(render_size.1 as f32) * 0.5),
        };
        let sign = match self.jitter_sign {
            JitterSign::Projection => 1.0,
            JitterSign::Sample => -1.0,
        };
        [sx * sign, sy * sign]
    }
}

fn safe_div(value: f32, by: f32) -> f32 {
    if by == 0.0 { 0.0 } else { value / by }
}

/// What a backend reads and in which conventions: the half of the boundary the frame checks
/// before it calls [`Upscaler::upscale`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputContract {
    /// Single-sample scene depth at the render size (near 0, far 1: Regia Engine's depth target).
    pub depth: InputNeed,
    /// Motion vectors at the render size (Regia Engine's velocity target).
    pub motion: InputNeed,
    /// Reactive mask (red, 0–1): how much to trust this frame over the history.
    pub reactive: InputNeed,
    /// Transparency-and-composition mask (red, 0–1); a backend without one folds it into the
    /// reactive mask, as TSR and FSR 3.1 do.
    pub transparency: InputNeed,
    /// The backend's own units for motion and jitter. The frame passes Regia Engine's; the backend
    /// converts with the [`InputConventions`] helpers.
    pub conventions: InputConventions,
    /// On Vulkan the runtime needs instance and device extensions requested at device creation
    /// (the device-creation hook of ADR-325 (6)).
    pub vulkan_device_hook: bool,
    /// The largest display ÷ render ratio per axis the backend accepts, in permille of 1
    /// (3000 = 3.0, Ultra Performance).
    pub max_ratio_permille: u32,
}

impl InputContract {
    /// TAAU: colour and (on a TAA frame) depth; it reads its own velocity target. The reactive
    /// mask is optional (ADR-2065); no transparency mask.
    pub const TAAU: Self = Self {
        depth: InputNeed::Optional,
        motion: InputNeed::Optional,
        reactive: InputNeed::Optional,
        transparency: InputNeed::Unused,
        conventions: InputConventions::REGIA,
        vulkan_device_hook: false,
        max_ratio_permille: 4000,
    };

    /// TSR and the FSR 3.1 port: depth required, motion and reactive optional, transparency
    /// folded into reactive.
    pub const TSR: Self = Self {
        depth: InputNeed::Required,
        motion: InputNeed::Optional,
        reactive: InputNeed::Optional,
        transparency: InputNeed::Optional,
        conventions: InputConventions::REGIA,
        vulkan_device_hook: false,
        max_ratio_permille: 3000,
    };

    /// A vendor runtime (DLSS, XeSS): depth and motion required, masks optional, render-pixel
    /// conventions, the Vulkan device hook (ADR-325).
    pub const VENDOR: Self = Self {
        depth: InputNeed::Required,
        motion: InputNeed::Required,
        reactive: InputNeed::Optional,
        transparency: InputNeed::Optional,
        conventions: InputConventions::RENDER_PIXELS,
        vulkan_device_hook: true,
        max_ratio_permille: 3000,
    };
}

/// Per-frame values a backend receives besides the images, in Regia Engine's conventions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UpscaleParams {
    /// Render (input) size.
    pub render_size: (u32, u32),
    /// Display (output) size.
    pub display_size: (u32, u32),
    /// This frame's projection jitter in NDC (`taau::halton_jitter`).
    pub jitter: [f32; 2],
    /// Drop the history (camera cut, first frame, size change).
    pub reset: bool,
    /// Exposure multiplier (linear HDR → display-referred).
    pub exposure: f32,
    /// The history was written at a pre-exposure this factor away from the current one
    /// (1 = unchanged).
    pub history_scale: f32,
    /// Frame time in milliseconds (some runtimes damp their history with it).
    ///
    /// [`Self::new`] sets this to 16.667. The render frame's vendor call uses that default and
    /// does not copy the measured frame delta, so DLSS is given one sixtieth of a second on every
    /// frame from that path.
    pub frame_time_ms: f32,
    /// Post-upscale sharpening, 0 (off) to 1.
    pub sharpness: f32,
}

impl UpscaleParams {
    /// A frame of `render_size` → `display_size` with no jitter, no reset, exposure 1, no
    /// sharpening, and `frame_time_ms` 16.667 (one sixtieth of a second, not a measured delta).
    #[must_use]
    pub const fn new(render_size: (u32, u32), display_size: (u32, u32)) -> Self {
        Self {
            render_size,
            display_size,
            jitter: [0.0, 0.0],
            reset: false,
            exposure: 1.0,
            history_scale: 1.0,
            frame_time_ms: 16.667,
            sharpness: 0.0,
        }
    }
}

/// One frame for an [`Upscaler`]: the images (of the target's image type) and the parameters.
#[derive(Debug)]
pub struct UpscaleFrame<'a, I: ?Sized> {
    /// Scene-referred HDR colour at the render size, jittered.
    pub color: &'a I,
    /// Scene depth at the render size.
    pub depth: Option<&'a I>,
    /// Motion vectors at the render size.
    pub motion: Option<&'a I>,
    /// Reactive mask at the render size.
    pub reactive: Option<&'a I>,
    /// Transparency-and-composition mask at the render size.
    pub transparency: Option<&'a I>,
    /// Sizes, jitter, reset and the rest.
    pub params: UpscaleParams,
}

impl<I: ?Sized> UpscaleFrame<'_, I> {
    /// Checks the frame against a contract: non-zero sizes, render no larger than display, the
    /// ratio within [`InputContract::max_ratio_permille`] and every required input present.
    /// Image sizes are the target's business (a wgpu view does not carry one).
    ///
    /// # Errors
    ///
    /// [`UpscaleError::InvalidSize`], [`UpscaleError::UnsupportedRatio`] or
    /// [`UpscaleError::MissingInput`].
    pub fn check(&self, contract: &InputContract) -> Result<(), UpscaleError> {
        let (rw, rh) = self.params.render_size;
        let (dw, dh) = self.params.display_size;
        if rw == 0 || rh == 0 || dw == 0 || dh == 0 || rw > dw || rh > dh {
            return Err(UpscaleError::InvalidSize {
                render: self.params.render_size,
                display: self.params.display_size,
            });
        }
        let max = u64::from(contract.max_ratio_permille).max(1);
        // The render axis may not be smaller than display × 1000 ÷ max rounded half up, the same
        // rounding `UpscaleQuality::render_size` uses: 1440p Ultra Performance renders 853 wide
        // (ratio 3.0012) and must pass a 3.0 limit.
        let min_axis = |display: u32| (u64::from(display) * 2000 + max) / (2 * max);
        if u64::from(rw) < min_axis(dw) || u64::from(rh) < min_axis(dh) {
            return Err(UpscaleError::UnsupportedRatio {
                render: self.params.render_size,
                display: self.params.display_size,
                max_ratio_permille: contract.max_ratio_permille,
            });
        }
        for (name, need, given) in [
            ("depth", contract.depth, self.depth.is_some()),
            ("motion", contract.motion, self.motion.is_some()),
            ("reactive", contract.reactive, self.reactive.is_some()),
            ("transparency", contract.transparency, self.transparency.is_some()),
        ] {
            if need == InputNeed::Required && !given {
                return Err(UpscaleError::MissingInput { input: name });
            }
        }
        Ok(())
    }
}

/// Why a backend refused or failed a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UpscaleError {
    /// A size is zero, or the render size is larger than the display.
    InvalidSize {
        /// Render size.
        render: (u32, u32),
        /// Display size.
        display: (u32, u32),
    },
    /// The display ÷ render ratio is beyond what the backend accepts.
    UnsupportedRatio {
        /// Render size.
        render: (u32, u32),
        /// Display size.
        display: (u32, u32),
        /// The backend's limit ([`InputContract::max_ratio_permille`]).
        max_ratio_permille: u32,
    },
    /// A required input is missing (`depth`, `motion`, `reactive` or `transparency`).
    MissingInput {
        /// Which one.
        input: &'static str,
    },
    /// An image is not the size the parameters say.
    SizeMismatch {
        /// Which image (`color`, `depth`, `motion`, `reactive`, `transparency`, `output`).
        input: &'static str,
        /// Its size.
        got: (u32, u32),
        /// The size it should have.
        expected: (u32, u32),
    },
    /// The backend's runtime failed (a vendor error, a lost device).
    Backend(String),
    /// The vendor runtime is not usable in this process (ADR-1150): not bundled, not found,
    /// unsupported on the device or too old a driver.
    RuntimeUnavailable {
        /// One of the runtime [`reason`] constants (`sdk_not_bundled`, `runtime_not_found`, ...).
        reason: &'static str,
    },
    /// The render size is outside the range the live vendor feature accepts (ADR-1150).
    RenderSizeOutOfRange {
        /// Render size.
        render: (u32, u32),
        /// The smallest size the feature accepts.
        min: (u32, u32),
        /// The largest size the feature accepts.
        max: (u32, u32),
    },
    /// The jitter is more than half a render pixel on an axis, or not finite: it was not given
    /// in NDC (ADR-1150).
    InvalidJitter {
        /// Render size the jitter was converted at.
        render: (u32, u32),
    },
    /// The backend gave up after this many failures in a row and stays off until it is
    /// re-enabled (ADR-1150, `vendor::MAX_CONSECUTIVE_FAILURES`).
    Disabled {
        /// Failures in a row.
        failures: u32,
    },
}

impl UpscaleError {
    /// Stable snake_case code for the agent protocol: `invalid_size`, `unsupported_ratio`,
    /// `missing_input`, `size_mismatch`, `backend_failed`, `runtime_unavailable`,
    /// `render_size_out_of_range`, `invalid_jitter`, `backend_disabled`.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidSize { .. } => "invalid_size",
            Self::UnsupportedRatio { .. } => "unsupported_ratio",
            Self::MissingInput { .. } => "missing_input",
            Self::SizeMismatch { .. } => "size_mismatch",
            Self::Backend(_) => "backend_failed",
            Self::RuntimeUnavailable { .. } => "runtime_unavailable",
            Self::RenderSizeOutOfRange { .. } => "render_size_out_of_range",
            Self::InvalidJitter { .. } => "invalid_jitter",
            Self::Disabled { .. } => "backend_disabled",
        }
    }
}

impl std::fmt::Display for UpscaleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSize { render, display } => {
                write!(f, "invalid upscale sizes: render {render:?}, display {display:?}")
            }
            Self::UnsupportedRatio {
                render,
                display,
                max_ratio_permille,
            } => write!(
                f,
                "render {render:?} to display {display:?} exceeds the ratio {max_ratio_permille}‰"
            ),
            Self::MissingInput { input } => write!(f, "the upscaler needs the {input} input"),
            Self::SizeMismatch { input, got, expected } => {
                write!(f, "{input} is {got:?}, expected {expected:?}")
            }
            Self::Backend(message) => write!(f, "upscaler backend failed: {message}"),
            Self::RuntimeUnavailable { reason } => write!(f, "the upscaler runtime is unavailable: {reason}"),
            Self::RenderSizeOutOfRange { render, min, max } => {
                write!(
                    f,
                    "render size {render:?} is outside the feature's range {min:?} to {max:?}"
                )
            }
            Self::InvalidJitter { render } => write!(
                f,
                "the jitter is more than half a render pixel at {render:?}: it must be NDC"
            ),
            Self::Disabled { failures } => {
                write!(f, "the upscaler is disabled after {failures} failures in a row")
            }
        }
    }
}

impl std::error::Error for UpscaleError {}

/// Where an [`Upscaler`]'s images live and what it records with.
pub trait UpscaleTarget {
    /// One input image.
    type Image: ?Sized;
    /// What the backend records or writes with, including the output.
    type Context<'c>;
}

/// wgpu texture views on the render device: the target the vendor backends implement.
#[derive(Debug, Clone, Copy)]
pub struct GpuTarget;

/// What a GPU backend records with.
pub struct GpuUpscaleContext<'c> {
    /// The render device.
    pub device: &'c wgpu::Device,
    /// Its queue (for uploads before the encoder is submitted).
    pub queue: &'c wgpu::Queue,
    /// The encoder of the upscale interval (ADR-325 (5): a vendor backend reaches its raw
    /// command buffer through wgpu-hal).
    pub encoder: &'c mut wgpu::CommandEncoder,
    /// The display-size output, created by Regia Engine (`Rgba16Float`, storage and sampled).
    pub output: &'c wgpu::TextureView,
    /// Command buffers the backend finished (a vendor runtime that records on a buffer of its
    /// own ends `encoder` here and pushes the finished part, then its own buffer). The caller
    /// submits them in order, before what `encoder` records next (ADR-2002).
    pub finished: &'c mut Vec<wgpu::CommandBuffer>,
}

impl UpscaleTarget for GpuTarget {
    type Image = wgpu::TextureView;
    type Context<'c> = GpuUpscaleContext<'c>;
}

/// CPU images: the software reference ([`super::reference`]) and its tests.
#[derive(Debug, Clone, Copy)]
pub struct CpuTarget;

/// What a CPU backend writes.
#[derive(Debug)]
pub struct CpuUpscaleContext<'c> {
    /// The display-size output; resized by the backend if it is not the display size.
    pub output: &'c mut super::reference::CpuImage,
}

impl UpscaleTarget for CpuTarget {
    type Image = super::reference::CpuImage;
    type Context<'c> = CpuUpscaleContext<'c>;
}

/// An upscaler behind the seam: anything that turns a jittered render-size frame into a
/// display-size one under an [`InputContract`].
///
/// The frame calls [`UpscaleFrame::check`] with [`Self::contract`] first, so `upscale` sees
/// only frames whose sizes and required inputs are in order.
pub trait Upscaler<T: UpscaleTarget> {
    /// Stable snake_case name (`dlss`, `xess`, `reference_bilinear`, ...).
    fn name(&self) -> &'static str;

    /// What the backend reads and in which conventions.
    fn contract(&self) -> InputContract;

    /// The render size the backend wants for a display size and mode. The built-ins and the
    /// reference use ADR-324's rule; a vendor runtime answers with its own optimal size.
    fn render_size(&self, display: (u32, u32), quality: UpscaleQuality) -> (u32, u32) {
        quality.render_size(display)
    }

    /// The render sizes the backend's runtime asks for at a display size and mode, or `None`
    /// when it has no opinion: the built-ins, the reference and a vendor that is not ready
    /// (ADR-2679). The frame and dynamic resolution stay inside this range while the backend runs.
    fn preferred_range(
        &self,
        _display: (u32, u32),
        _quality: UpscaleQuality,
    ) -> Option<super::vendor::PreferredRange> {
        None
    }

    /// Upscales one frame into the context's output.
    ///
    /// # Errors
    ///
    /// Any [`UpscaleError`]; on error the output's contents are unspecified and the frame
    /// should fall back to a built-in upscaler.
    fn upscale(
        &mut self,
        ctx: &mut T::Context<'_>,
        frame: &UpscaleFrame<'_, T::Image>,
    ) -> Result<(), UpscaleError>;
}

/// The state of a vendor runtime library in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendorRuntime {
    /// Part of Regia Engine (TAAU, TSR, FSR 3.1): nothing to load.
    CompiledIn,
    /// This build has no code for the vendor's runtime ([`reason::SDK_NOT_BUNDLED`]).
    NotBundled,
    /// The integration is built but its library was not found ([`reason::RUNTIME_NOT_FOUND`]).
    NotFound,
    /// The integration is built and its library was found, but no render device has
    /// initialised it yet ([`reason::RUNTIME_NOT_INITIALIZED`], ADR-2002).
    Found,
    /// The library loaded and reports the feature unusable on this device
    /// ([`reason::RUNTIME_REPORTS_UNSUPPORTED`]).
    ReportsUnsupported,
    /// The library loaded and wants a newer driver ([`reason::NEEDS_NEWER_DRIVER`]).
    NeedsNewerDriver,
    /// Loaded and ready.
    Ready,
    /// The library was found but did not start on this device, and neither the GPU nor the
    /// driver is the reason ([`reason::RUNTIME_INIT_FAILED`], ADR-2679).
    InitFailed,
}

impl VendorRuntime {
    /// Stable snake_case name for the agent protocol: `compiled_in`, `not_bundled`,
    /// `not_found`, `found`, `reports_unsupported`, `needs_newer_driver`, `ready`, `init_failed`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CompiledIn => "compiled_in",
            Self::NotBundled => "not_bundled",
            Self::NotFound => "not_found",
            Self::Found => "found",
            Self::ReportsUnsupported => "reports_unsupported",
            Self::NeedsNewerDriver => "needs_newer_driver",
            Self::Ready => "ready",
            Self::InitFailed => "init_failed",
        }
    }

    /// The blocker this state adds, if any.
    #[must_use]
    pub const fn blocker(self) -> Option<&'static str> {
        match self {
            Self::CompiledIn | Self::Ready => None,
            Self::NotBundled => Some(reason::SDK_NOT_BUNDLED),
            Self::NotFound => Some(reason::RUNTIME_NOT_FOUND),
            Self::Found => Some(reason::RUNTIME_NOT_INITIALIZED),
            Self::ReportsUnsupported => Some(reason::RUNTIME_REPORTS_UNSUPPORTED),
            Self::NeedsNewerDriver => Some(reason::NEEDS_NEWER_DRIVER),
            Self::InitFailed => Some(reason::RUNTIME_INIT_FAILED),
        }
    }
}

/// The state of every vendor runtime, for [`available_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorRuntimes {
    /// DLSS (NGX).
    pub dlss: VendorRuntime,
    /// XeSS.
    pub xess: VendorRuntime,
}

impl VendorRuntimes {
    /// This build: no vendor code, so both are [`VendorRuntime::NotBundled`].
    pub const THIS_BUILD: Self = Self {
        dlss: VendorRuntime::NotBundled,
        xess: VendorRuntime::NotBundled,
    };

    /// What this process knows now: DLSS from [`super::vendor::dlss::dlss_status`] (the
    /// same as [`Self::THIS_BUILD`] without the `dlss` feature), and XeSS from its device hook (ADR-4656).
    #[must_use]
    pub fn current() -> Self {
        Self {
            dlss: super::vendor::dlss::dlss_status().runtime,
            xess: super::vendor::xess::xess_status().runtime,
        }
    }

    /// The runtime state of a vendor.
    #[must_use]
    pub const fn get(&self, vendor: VendorUpscaler) -> VendorRuntime {
        match vendor {
            VendorUpscaler::Dlss => self.dlss,
            VendorUpscaler::Xess => self.xess,
        }
    }
}

/// The device a probe looks at.
#[derive(Debug, Clone, Copy)]
pub struct ProbeDevice<'a> {
    /// The adapter (vendor id, device type, name).
    pub adapter: &'a wgpu::AdapterInfo,
    /// The backend the device runs on (normally `adapter.backend`).
    pub backend: wgpu::Backend,
    /// What device creation enabled beyond wgpu's features.
    pub features: UpscalerFeatures,
}

/// One backend's feature probe: what it needs from the device, checked without loading
/// anything.
pub trait UpscalerProbe: Sync {
    /// The backend.
    fn kind(&self) -> UpscalerKind;

    /// What the backend reads.
    fn contract(&self) -> InputContract;

    /// The device's blockers, most fundamental first: hardware, then API, then extensions.
    fn device_blockers(&self, device: &ProbeDevice<'_>) -> Vec<&'static str>;

    /// The status on `device` (or with no device) with the runtime in `runtime`: the device's
    /// blockers (or [`reason::NO_RENDER_DEVICE`] for a backend that needs a runtime), then the
    /// runtime's.
    fn status(&self, device: Option<&ProbeDevice<'_>>, runtime: VendorRuntime) -> UpscalerStatus {
        let mut blockers = match device {
            Some(device) => self.device_blockers(device),
            None if runtime == VendorRuntime::CompiledIn => Vec::new(),
            None => vec![reason::NO_RENDER_DEVICE],
        };
        blockers.extend(runtime.blocker());
        UpscalerStatus::from_blockers(self.kind(), blockers)
    }
}

/// The probe of a built-in upscaler: it runs on every device Regia Engine renders on.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinProbe(pub TemporalUpscaler);

impl UpscalerProbe for BuiltinProbe {
    fn kind(&self) -> UpscalerKind {
        UpscalerKind::Builtin(self.0)
    }

    fn contract(&self) -> InputContract {
        match self.0 {
            TemporalUpscaler::Taau => InputContract::TAAU,
            TemporalUpscaler::Tsr | TemporalUpscaler::Fsr3 => InputContract::TSR,
        }
    }

    fn device_blockers(&self, _device: &ProbeDevice<'_>) -> Vec<&'static str> {
        Vec::new()
    }
}

fn api_blockers(device: &ProbeDevice<'_>, extensions_enabled: bool, out: &mut Vec<&'static str>) {
    match device.backend {
        wgpu::Backend::Vulkan => {
            if !extensions_enabled {
                out.push(reason::NEEDS_DX12_OR_VULKAN_EXTENSION);
            }
        }
        wgpu::Backend::Dx12 => {}
        _ => out.push(reason::NEEDS_DX12_OR_VULKAN),
    }
}

/// DLSS Super Resolution: NVIDIA adapters only, Vulkan with the runtime's extensions. The binding
/// has no DX12 path, so a DX12 device is blocked with [`reason::DLSS_NEEDS_VULKAN`] (ADR-7695).
#[derive(Debug, Clone, Copy)]
pub struct DlssProbe;

impl UpscalerProbe for DlssProbe {
    fn kind(&self) -> UpscalerKind {
        UpscalerKind::Vendor(VendorUpscaler::Dlss)
    }

    fn contract(&self) -> InputContract {
        InputContract::VENDOR
    }

    fn device_blockers(&self, device: &ProbeDevice<'_>) -> Vec<&'static str> {
        let mut out = Vec::new();
        if device.adapter.vendor != vendor_id::NVIDIA {
            out.push(reason::NEEDS_NVIDIA_ADAPTER);
        }
        if device.backend == wgpu::Backend::Dx12 {
            out.push(reason::DLSS_NEEDS_VULKAN);
        } else {
            api_blockers(device, device.features.dlss_vulkan_extensions, &mut out);
        }
        out
    }
}

/// XeSS Super Resolution: any hardware desktop GPU (Intel's XMX path or the DP4a path), Vulkan
/// with the runtime's extensions and features (ADR-4656).
#[derive(Debug, Clone, Copy)]
pub struct XessProbe;

impl UpscalerProbe for XessProbe {
    fn kind(&self) -> UpscalerKind {
        UpscalerKind::Vendor(VendorUpscaler::Xess)
    }

    fn contract(&self) -> InputContract {
        InputContract {
            reactive: InputNeed::Unused,
            transparency: InputNeed::Unused,
            ..InputContract::VENDOR
        }
    }

    fn device_blockers(&self, device: &ProbeDevice<'_>) -> Vec<&'static str> {
        let mut out = Vec::new();
        if device.adapter.device_type == wgpu::DeviceType::Cpu {
            out.push(reason::NEEDS_HARDWARE_ADAPTER);
        }
        if matches!(
            device.adapter.vendor,
            vendor_id::QUALCOMM | vendor_id::ARM | vendor_id::IMAGINATION | vendor_id::APPLE
        ) {
            out.push(reason::NEEDS_DESKTOP_GPU);
        }
        if device.backend == wgpu::Backend::Vulkan {
            api_blockers(device, device.features.xess_vulkan_extensions, &mut out);
        } else {
            out.push(reason::XESS_NEEDS_VULKAN);
        }
        out
    }
}

static TAAU_PROBE: BuiltinProbe = BuiltinProbe(TemporalUpscaler::Taau);
static TSR_PROBE: BuiltinProbe = BuiltinProbe(TemporalUpscaler::Tsr);
static FSR3_PROBE: BuiltinProbe = BuiltinProbe(TemporalUpscaler::Fsr3);
static DLSS_PROBE: DlssProbe = DlssProbe;
static XESS_PROBE: XessProbe = XessProbe;

/// Every backend's probe, in [`UpscalerKind::ALL`] order.
#[must_use]
pub fn probes() -> [&'static dyn UpscalerProbe; 5] {
    [&TAAU_PROBE, &TSR_PROBE, &FSR3_PROBE, &DLSS_PROBE, &XESS_PROBE]
}

/// The probe of one backend.
#[must_use]
pub fn probe_for(kind: UpscalerKind) -> Option<&'static dyn UpscalerProbe> {
    probes().into_iter().find(|probe| probe.kind() == kind)
}

/// Every backend's status on `device` (or with none) with the vendor runtimes in `runtimes`,
/// in [`UpscalerKind::ALL`] order. [`super::available`] is this with
/// [`VendorRuntimes::THIS_BUILD`].
#[must_use]
pub fn available_with(device: Option<&ProbeDevice<'_>>, runtimes: &VendorRuntimes) -> Vec<UpscalerStatus> {
    probes()
        .into_iter()
        .map(|probe| {
            let runtime = match probe.kind() {
                UpscalerKind::Vendor(vendor) => runtimes.get(vendor),
                UpscalerKind::Builtin(_) => VendorRuntime::CompiledIn,
            };
            probe.status(device, runtime)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1.0e-5 && (a[1] - b[1]).abs() < 1.0e-5
    }

    #[test]
    fn regia_conventions_are_the_identity() {
        let c = InputConventions::REGIA;
        assert_eq!(c.motion_scale_from_regia((1920, 1080)), [1.0, 1.0]);
        let mv = [0.013, -0.25];
        assert_eq!(c.motion_from_regia(mv, (1920, 1080)), mv);
        let jitter = [0.001, -0.0007];
        assert_eq!(c.jitter_from_regia(jitter, (1920, 1080)), jitter);
    }

    #[test]
    fn render_pixel_conventions_convert_exactly_and_round_trip() {
        let c = InputConventions::RENDER_PIXELS;
        // Half the image right and a quarter up is 32 and −8 render pixels at 64×32.
        assert!(close(c.motion_from_regia([0.5, -0.25], (64, 32)), [32.0, -8.0]));
        assert_eq!(c.motion_scale_from_regia((64, 32)), [64.0, 32.0]);
        // One pixel of NDC jitter right and up is +1 px in x and −1 px in y (pixel y is down).
        assert!(close(
            c.jitter_from_regia([2.0 / 64.0, 2.0 / 32.0], (64, 32)),
            [1.0, -1.0]
        ));
        // Every combination round-trips, and each flag flips exactly its own sign.
        let render = (1707, 960);
        let mv = [0.0123, -0.0456];
        let jitter = crate::taau::halton_jitter(5, render.0, render.1);
        for motion_units in [MotionUnits::Uv, MotionUnits::RenderPixels] {
            for motion_direction in [MotionDirection::ToPrevious, MotionDirection::ToCurrent] {
                for jitter_units in [JitterUnits::Ndc, JitterUnits::RenderPixels] {
                    for jitter_sign in [JitterSign::Projection, JitterSign::Sample] {
                        let c = InputConventions {
                            motion_units,
                            motion_direction,
                            jitter_units,
                            jitter_sign,
                        };
                        let there = c.motion_from_regia(mv, render);
                        assert!(close(c.motion_to_regia(there, render), mv), "{c:?}");
                        let j = c.jitter_from_regia(jitter, render);
                        assert!(close(c.jitter_to_regia(j, render), jitter), "{c:?}");
                        let flipped = InputConventions {
                            motion_direction: match motion_direction {
                                MotionDirection::ToPrevious => MotionDirection::ToCurrent,
                                MotionDirection::ToCurrent => MotionDirection::ToPrevious,
                            },
                            jitter_sign: match jitter_sign {
                                JitterSign::Projection => JitterSign::Sample,
                                JitterSign::Sample => JitterSign::Projection,
                            },
                            ..c
                        };
                        let m2 = flipped.motion_from_regia(mv, render);
                        assert!(close(m2, [-there[0], -there[1]]), "{c:?}");
                        let j2 = flipped.jitter_from_regia(jitter, render);
                        assert!(close(j2, [-j[0], -j[1]]), "{c:?}");
                    }
                }
            }
        }
        // A zero-size axis converts back to zero instead of dividing by zero.
        assert_eq!(c.motion_to_regia([3.0, 4.0], (0, 0)), [0.0, 0.0]);
    }

    fn frame(render: (u32, u32), display: (u32, u32)) -> UpscaleFrame<'static, ()> {
        UpscaleFrame {
            color: &(),
            depth: None,
            motion: None,
            reactive: None,
            transparency: None,
            params: UpscaleParams::new(render, display),
        }
    }

    #[test]
    fn the_contract_check_refuses_bad_frames_with_stable_codes() {
        let ok = frame((1280, 720), (2560, 1440));
        assert_eq!(ok.check(&InputContract::TAAU), Ok(()));
        for bad in [
            frame((0, 720), (2560, 1440)),
            frame((1280, 720), (2560, 0)),
            frame((2561, 1440), (2560, 1440)),
        ] {
            let error = bad.check(&InputContract::TAAU).err();
            assert_eq!(error.as_ref().map(UpscaleError::code), Some("invalid_size"));
        }
        // Ultra Performance (3.0) is the vendor limit; 4× is refused, 3× passes once the
        // required inputs are there.
        let four = frame((640, 360), (2560, 1440));
        assert_eq!(
            four.check(&InputContract::VENDOR).err().map(|e| e.code()),
            Some("unsupported_ratio")
        );
        assert_eq!(four.check(&InputContract::TAAU), Ok(()), "TAAU goes to 4×");
        let three = frame((1280, 720), (3840, 2160));
        assert_eq!(
            three.check(&InputContract::VENDOR),
            Err(UpscaleError::MissingInput { input: "depth" })
        );
        let with_depth = UpscaleFrame {
            depth: Some(&()),
            ..frame((1280, 720), (3840, 2160))
        };
        assert_eq!(
            with_depth.check(&InputContract::VENDOR),
            Err(UpscaleError::MissingInput { input: "motion" })
        );
        assert_eq!(with_depth.check(&InputContract::TSR), Ok(()));
        let full = UpscaleFrame {
            motion: Some(&()),
            ..with_depth
        };
        assert_eq!(full.check(&InputContract::VENDOR), Ok(()));
        // Ratio limits are exact: 1707×960 → 2560×1440 is 1.4997 and passes a 1.5 limit.
        let tight = InputContract {
            max_ratio_permille: 1500,
            ..InputContract::TAAU
        };
        assert_eq!(frame((1707, 960), (2560, 1440)).check(&tight), Ok(()));
        assert!(frame((1706, 960), (2560, 1440)).check(&tight).is_err());
        // Every mode's own render size passes the vendor limit, Ultra Performance included
        // (853×480 is a 3.0012 ratio at 1440p), at the sizes players use.
        for display in [(2560, 1440), (3840, 2160), (1920, 1080), (1280, 800), (64, 64)] {
            for mode in UpscaleQuality::ALL {
                let render = mode.render_size(display);
                let full = UpscaleFrame {
                    depth: Some(&()),
                    motion: Some(&()),
                    ..frame(render, display)
                };
                assert_eq!(full.check(&InputContract::VENDOR), Ok(()), "{mode:?} {display:?}");
            }
        }
        // One pixel below the rounded size is refused.
        assert_eq!(
            frame((852, 480), (2560, 1440))
                .check(&InputContract::TSR)
                .err()
                .map(|e| e.code()),
            Some("unsupported_ratio")
        );
        for error in [
            UpscaleError::InvalidSize {
                render: (0, 0),
                display: (0, 0),
            },
            UpscaleError::UnsupportedRatio {
                render: (1, 1),
                display: (4, 4),
                max_ratio_permille: 3000,
            },
            UpscaleError::MissingInput { input: "depth" },
            UpscaleError::SizeMismatch {
                input: "color",
                got: (1, 1),
                expected: (2, 2),
            },
            UpscaleError::Backend("lost".into()),
            UpscaleError::RuntimeUnavailable {
                reason: reason::RUNTIME_NOT_FOUND,
            },
            UpscaleError::RenderSizeOutOfRange {
                render: (1, 1),
                min: (2, 2),
                max: (3, 3),
            },
            UpscaleError::InvalidJitter { render: (8, 8) },
            UpscaleError::Disabled { failures: 3 },
        ] {
            let code = error.code();
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
            assert!(!error.to_string().is_empty());
        }
    }

    fn adapter(vendor: u32, device_type: wgpu::DeviceType, backend: wgpu::Backend) -> wgpu::AdapterInfo {
        let mut info = wgpu::AdapterInfo::new(device_type, backend);
        info.vendor = vendor;
        info
    }

    #[test]
    fn every_kind_has_one_probe_and_a_contract() {
        let kinds: Vec<UpscalerKind> = probes().iter().map(|p| p.kind()).collect();
        assert_eq!(kinds, UpscalerKind::ALL.to_vec());
        for kind in UpscalerKind::ALL {
            let probe = probe_for(kind).map(|p| p.kind());
            assert_eq!(probe, Some(kind));
            let contract = probe_for(kind).map(|p| p.contract());
            let vendor = matches!(kind, UpscalerKind::Vendor(_));
            assert_eq!(contract.map(|c| c.vulkan_device_hook), Some(vendor), "{kind:?}");
            let native = contract.map(|c| c.conventions == InputConventions::REGIA);
            assert_eq!(native, Some(!vendor), "{kind:?}");
        }
        // TSR and FSR 3.1 need depth; TAAU does not.
        let tsr = probe_for(UpscalerKind::Builtin(TemporalUpscaler::Tsr)).map(|p| p.contract().depth);
        assert_eq!(tsr, Some(InputNeed::Required));
        let taau = probe_for(UpscalerKind::Builtin(TemporalUpscaler::Taau)).map(|p| p.contract().depth);
        assert_eq!(taau, Some(InputNeed::Optional));
        for need in [InputNeed::Required, InputNeed::Optional, InputNeed::Unused] {
            assert!(need.name().bytes().all(|b| b.is_ascii_lowercase()));
        }
    }

    fn blockers(list: &[UpscalerStatus], vendor: VendorUpscaler) -> Vec<&'static str> {
        list.iter()
            .find(|s| s.kind == UpscalerKind::Vendor(vendor))
            .map(|s| s.blockers.clone())
            .unwrap_or_default()
    }

    #[test]
    fn xess_ready_on_another_device_does_not_enable_dx12() {
        let info = adapter(0x8086, wgpu::DeviceType::IntegratedGpu, wgpu::Backend::Dx12);
        let device = ProbeDevice {
            adapter: &info,
            backend: wgpu::Backend::Dx12,
            features: UpscalerFeatures {
                xess_vulkan_extensions: true,
                ..Default::default()
            },
        };
        let status = XessProbe.status(Some(&device), VendorRuntime::Ready);
        assert!(!status.available);
        assert_eq!(status.blockers, vec![reason::XESS_NEEDS_VULKAN]);
        assert_eq!(status.unavailable_code(), Some(reason::XESS_NEEDS_VULKAN));
    }

    /// The DLSS binding drives NGX through Vulkan only (ADR-7695): a ready runtime from another
    /// device must not make a DX12 device look able to run it, and the row names the API.
    #[test]
    fn dlss_ready_on_another_device_does_not_enable_dx12() {
        let info = adapter(0x10DE, wgpu::DeviceType::DiscreteGpu, wgpu::Backend::Dx12);
        let device = ProbeDevice {
            adapter: &info,
            backend: wgpu::Backend::Dx12,
            features: UpscalerFeatures {
                dlss_vulkan_extensions: true,
                ..Default::default()
            },
        };
        let status = DlssProbe.status(Some(&device), VendorRuntime::Ready);
        assert!(!status.available);
        assert_eq!(status.blockers, vec![reason::DLSS_NEEDS_VULKAN]);
        assert_eq!(status.unavailable_code(), Some(reason::DLSS_NEEDS_VULKAN));
        // A runtime problem is still the code to act on first, as for every vendor.
        let status = DlssProbe.status(Some(&device), VendorRuntime::NotFound);
        assert_eq!(
            status.blockers,
            vec![reason::DLSS_NEEDS_VULKAN, reason::RUNTIME_NOT_FOUND]
        );
        assert_eq!(status.unavailable_code(), Some(reason::RUNTIME_NOT_FOUND));
        // The same adapter on Vulkan with NGX's extensions runs.
        let vulkan = adapter(0x10DE, wgpu::DeviceType::DiscreteGpu, wgpu::Backend::Vulkan);
        let device = ProbeDevice {
            adapter: &vulkan,
            backend: wgpu::Backend::Vulkan,
            features: UpscalerFeatures {
                dlss_vulkan_extensions: true,
                ..Default::default()
            },
        };
        assert!(DlssProbe.status(Some(&device), VendorRuntime::Ready).available);
    }

    #[test]
    fn vendor_runtime_states_add_their_own_reason() {
        // An NVIDIA Vulkan device with NGX's extensions: the device adds no blocker of its own,
        // so each row carries only its runtime's reason.
        let nvidia = adapter(0x10DE, wgpu::DeviceType::DiscreteGpu, wgpu::Backend::Vulkan);
        let device = ProbeDevice {
            adapter: &nvidia,
            backend: wgpu::Backend::Vulkan,
            features: UpscalerFeatures {
                dlss_vulkan_extensions: true,
                ..Default::default()
            },
        };
        for (runtime, expected) in [
            (VendorRuntime::NotBundled, vec![reason::SDK_NOT_BUNDLED]),
            (VendorRuntime::NotFound, vec![reason::RUNTIME_NOT_FOUND]),
            (VendorRuntime::Found, vec![reason::RUNTIME_NOT_INITIALIZED]),
            (
                VendorRuntime::ReportsUnsupported,
                vec![reason::RUNTIME_REPORTS_UNSUPPORTED],
            ),
            (VendorRuntime::NeedsNewerDriver, vec![reason::NEEDS_NEWER_DRIVER]),
            (VendorRuntime::InitFailed, vec![reason::RUNTIME_INIT_FAILED]),
            (VendorRuntime::Ready, vec![]),
        ] {
            let runtimes = VendorRuntimes {
                dlss: runtime,
                xess: VendorRuntime::NotBundled,
            };
            let list = available_with(Some(&device), &runtimes);
            assert_eq!(blockers(&list, VendorUpscaler::Dlss), expected, "{runtime:?}");
            assert_eq!(
                blockers(&list, VendorUpscaler::Xess),
                vec![reason::NEEDS_DX12_OR_VULKAN_EXTENSION, reason::SDK_NOT_BUNDLED],
                "the other vendor keeps its own state and extension requirement"
            );
            let dlss = list
                .iter()
                .find(|s| s.kind == UpscalerKind::Vendor(VendorUpscaler::Dlss))
                .map(|s| (s.available, s.reason));
            assert_eq!(dlss, Some((expected.is_empty(), expected.first().copied())));
        }
        // A ready runtime does not hide what the device lacks: XeSS on lavapipe.
        let lavapipe = adapter(0x10005, wgpu::DeviceType::Cpu, wgpu::Backend::Vulkan);
        let device = ProbeDevice {
            adapter: &lavapipe,
            backend: wgpu::Backend::Vulkan,
            features: UpscalerFeatures::default(),
        };
        let ready = VendorRuntimes {
            dlss: VendorRuntime::Ready,
            xess: VendorRuntime::Ready,
        };
        let list = available_with(Some(&device), &ready);
        assert_eq!(
            blockers(&list, VendorUpscaler::Xess),
            vec![
                reason::NEEDS_HARDWARE_ADAPTER,
                reason::NEEDS_DX12_OR_VULKAN_EXTENSION
            ]
        );
        // No device: a vendor kind is blocked by that alone once its runtime is ready; the
        // built-ins are available.
        let list = available_with(None, &ready);
        assert_eq!(
            blockers(&list, VendorUpscaler::Dlss),
            vec![reason::NO_RENDER_DEVICE]
        );
        assert!(list.iter().take(3).all(|s| s.available));
        // This process's runtimes reproduce `available` exactly. An SDK-free build's runtimes
        // are `THIS_BUILD`; a linked build reports its runtime's state instead (ADR-4907).
        let info = adapter(0x10DE, wgpu::DeviceType::DiscreteGpu, wgpu::Backend::Vulkan);
        let device = ProbeDevice {
            adapter: &info,
            backend: wgpu::Backend::Vulkan,
            features: UpscalerFeatures::default(),
        };
        assert_eq!(
            available_with(Some(&device), &VendorRuntimes::current()),
            super::super::available(&info, wgpu::Backend::Vulkan, &UpscalerFeatures::default())
        );
        let current = VendorRuntimes::current();
        assert_eq!(
            current.dlss == VendorRuntime::NotBundled,
            !super::super::vendor::dlss::dlss_status().binding_built
        );
        assert_eq!(
            current.xess == VendorRuntime::NotBundled,
            !super::super::vendor::xess::BINDING_BUILT
        );
        for code in [
            reason::RUNTIME_NOT_FOUND,
            reason::RUNTIME_REPORTS_UNSUPPORTED,
            reason::NEEDS_NEWER_DRIVER,
        ] {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }

    /// ADR-2679: each way a vendor cannot run has one stable code, whatever else is missing.
    /// The GTX row mocks the adapter and NGX's answer: an NVIDIA card NGX refuses, on a device
    /// with or without NGX's extensions.
    #[test]
    fn every_way_a_vendor_cannot_run_has_one_stable_code() {
        let code_on = |adapter_info: &wgpu::AdapterInfo, hooked: bool, dlss: VendorRuntime| {
            let device = ProbeDevice {
                adapter: adapter_info,
                backend: adapter_info.backend,
                features: UpscalerFeatures {
                    dlss_vulkan_extensions: hooked,
                    xess_vulkan_extensions: hooked,
                },
            };
            let runtimes = VendorRuntimes {
                dlss,
                xess: VendorRuntime::NotBundled,
            };
            available_with(Some(&device), &runtimes)
                .into_iter()
                .find(|s| s.kind == UpscalerKind::Vendor(VendorUpscaler::Dlss))
                .and_then(|s| s.unavailable_code())
        };
        let mut gtx1080 = adapter(0x10DE, wgpu::DeviceType::DiscreteGpu, wgpu::Backend::Vulkan);
        gtx1080.name = "NVIDIA GeForce GTX 1080".to_string();
        let rtx = adapter(0x10DE, wgpu::DeviceType::DiscreteGpu, wgpu::Backend::Vulkan);
        let amd = adapter(0x1002, wgpu::DeviceType::DiscreteGpu, wgpu::Backend::Vulkan);
        for hooked in [false, true] {
            // No SDK: on Vulkan a build without the SDK also has no extensions; that is not the news.
            assert_eq!(
                code_on(&rtx, hooked, VendorRuntime::NotBundled),
                Some(reason::SDK_NOT_BUNDLED)
            );
            assert_eq!(
                code_on(&rtx, hooked, VendorRuntime::NotFound),
                Some(reason::RUNTIME_NOT_FOUND)
            );
            // The GPU NGX refuses.
            assert_eq!(
                code_on(&gtx1080, hooked, VendorRuntime::ReportsUnsupported),
                Some(reason::RUNTIME_REPORTS_UNSUPPORTED)
            );
            assert_eq!(
                code_on(&rtx, hooked, VendorRuntime::NeedsNewerDriver),
                Some(reason::NEEDS_NEWER_DRIVER)
            );
            assert_eq!(
                code_on(&rtx, hooked, VendorRuntime::InitFailed),
                Some(reason::RUNTIME_INIT_FAILED)
            );
            // A GPU that can never run it says so before anything about the SDK.
            assert_eq!(
                code_on(&amd, hooked, VendorRuntime::NotBundled),
                Some(reason::NEEDS_NVIDIA_ADAPTER)
            );
        }
        // Ready on a device without the extensions: the extensions are the news.
        assert_eq!(
            code_on(&rtx, false, VendorRuntime::Ready),
            Some(reason::NEEDS_DX12_OR_VULKAN_EXTENSION)
        );
        assert_eq!(code_on(&rtx, true, VendorRuntime::Ready), None);
        // `reason` keeps the probe's order; only the new field reorders.
        let device = ProbeDevice {
            adapter: &rtx,
            backend: wgpu::Backend::Vulkan,
            features: UpscalerFeatures::default(),
        };
        let row = available_with(Some(&device), &VendorRuntimes::THIS_BUILD)
            .into_iter()
            .find(|s| s.kind == UpscalerKind::Vendor(VendorUpscaler::Dlss));
        assert_eq!(
            row.as_ref().and_then(|s| s.reason),
            Some(reason::NEEDS_DX12_OR_VULKAN_EXTENSION)
        );
        assert_eq!(
            row.and_then(|s| s.unavailable_code()),
            Some(reason::SDK_NOT_BUNDLED)
        );
        assert_eq!(VendorRuntime::InitFailed.name(), "init_failed");
    }

    /// A stand-in GPU backend: proves the GPU half of the boundary is implementable and
    /// object-safe without a device.
    struct FakeVendor;

    impl Upscaler<GpuTarget> for FakeVendor {
        fn name(&self) -> &'static str {
            "fake_vendor"
        }

        fn contract(&self) -> InputContract {
            InputContract::VENDOR
        }

        fn render_size(&self, display: (u32, u32), _quality: UpscaleQuality) -> (u32, u32) {
            // A runtime that answers with its own optimal size.
            (display.0 / 2, display.1 / 2)
        }

        fn upscale(
            &mut self,
            _ctx: &mut GpuUpscaleContext<'_>,
            _frame: &UpscaleFrame<'_, wgpu::TextureView>,
        ) -> Result<(), UpscaleError> {
            Err(UpscaleError::Backend("no device in this test".into()))
        }
    }

    #[test]
    fn a_gpu_backend_plugs_into_the_boundary() {
        let backends: Vec<Box<dyn Upscaler<GpuTarget>>> = vec![Box::new(FakeVendor)];
        for backend in &backends {
            assert_eq!(backend.name(), "fake_vendor");
            assert!(backend.contract().vulkan_device_hook);
            assert_eq!(
                backend.render_size((3840, 2160), UpscaleQuality::Quality),
                (1920, 1080)
            );
        }
        // The CPU reference uses the default size rule (ADR-324).
        let reference = crate::upscaler::reference::ReferenceUpscaler::new(Default::default());
        let cpu: &dyn Upscaler<CpuTarget> = &reference;
        assert_eq!(
            cpu.render_size((3840, 2160), UpscaleQuality::Quality),
            (2560, 1440)
        );
    }
}
