// SPDX-License-Identifier: MPL-2.0
//! Swapchain plan: the format, color space, present mode, alpha mode and frame latency of a window
//! surface, chosen from the surface capabilities, [`OutputSettings`](crate::OutputSettings), the
//! window's present mode, the backend and what the display reports (ADR-525).
//!
//! [`plan_swapchain`] is pure, so the DX12 rules run in Linux tests:
//! - **HDR on an SDR display.** DX12 lists its HDR color spaces whether or not Windows HDR is on
//!   (wgpu-hal 30, `dx12/adapter.rs`); the desktop compositor then tone-maps a PQ or scRGB
//!   swapchain down to SDR. An HDR request falls back to SDR when the display reports that it is
//!   not in an HDR mode ([`HdrFallback::DisplaySdr`]). An unknown display state keeps the request.
//! - **HDR-only formats.** A format that the surface lists only with an HDR color space is not in
//!   `wgpu::SurfaceCapabilities::formats` (that list holds the formats `Auto` can configure), so
//!   the HDR pairs are looked up in the per-format color spaces instead.
//! - **VRR on DX12** presents with `Immediate`: sync interval 0 with `DXGI_PRESENT_ALLOW_TEARING`,
//!   which is what variable refresh needs on a DXGI flip-model swapchain (Microsoft Learn,
//!   "Variable refresh rate displays"). Without tearing support DX12 has no `Immediate` and VRR
//!   takes `Mailbox`, as on the other backends.
//! - **Alpha** is `Opaque` when the surface lists it: a game window never blends with the desktop.
//! - **Frame latency** is [`FRAME_LATENCY`] on every backend (DXGI `SetMaximumFrameLatency`, the
//!   Vulkan swapchain image count).
//!
//! [`SwapchainInfo`] is what a live window surface was configured with; `render.swapchain` (book
//! `agents/swapchain.md`) reports it.

use crate::calibration::DisplayLuminance;
use crate::output::{HdrOutput, PresentPolicy, select_present_mode, select_surface_format};

/// Frames the CPU may queue ahead of the display (`desired_maximum_frame_latency`).
pub const FRAME_LATENCY: u32 = 2;

/// Why an HDR request was configured as SDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HdrFallback {
    /// The surface does not list the format and color-space pair of the request.
    NotOffered,
    /// The display reports that it is not in an HDR mode (Windows HDR is off).
    DisplaySdr,
}

impl HdrFallback {
    /// Stable lowercase code for the agent protocol (`not_offered`, `display_sdr`).
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::NotOffered => "not_offered",
            Self::DisplaySdr => "display_sdr",
        }
    }
}

/// The inputs of [`plan_swapchain`] besides the surface capabilities.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwapchainRequest {
    /// Backend of the adapter that presents.
    pub backend: wgpu::Backend,
    /// Present policy from [`OutputSettings`](crate::OutputSettings).
    pub present: PresentPolicy,
    /// The window's own present mode, used by [`PresentPolicy::Vsync`].
    pub window_present: regia_platform::PresentMode,
    /// HDR request from [`OutputSettings`](crate::OutputSettings).
    pub hdr: HdrOutput,
    /// Whether the display is in an HDR mode: `Some(false)` is an SDR display (Windows HDR off),
    /// `None` is unknown (no report on this platform).
    pub display_hdr: Option<bool>,
}

/// What a window surface is configured with.
#[derive(Debug, Clone, PartialEq)]
pub struct SwapchainPlan {
    /// Swapchain texture format.
    pub format: wgpu::TextureFormat,
    /// Color space paired with the format (`Auto` for SDR).
    pub color_space: wgpu::SurfaceColorSpace,
    /// Present mode handed to wgpu (an `Auto*` mode is resolved by wgpu, see
    /// [`effective_present_mode`]).
    pub present_mode: wgpu::PresentMode,
    /// Composite alpha mode.
    pub alpha_mode: wgpu::CompositeAlphaMode,
    /// Maximum frame latency.
    pub frame_latency: u32,
    /// The swapchain can be a copy source (screenshots).
    pub copy_src: bool,
    /// The configured color space is HDR.
    pub hdr_active: bool,
    /// Why an HDR request ended as SDR; `None` when HDR is active or was not requested.
    pub hdr_fallback: Option<HdrFallback>,
}

impl SwapchainPlan {
    /// The wgpu surface configuration for a `width`×`height` window.
    #[must_use]
    pub fn configuration(&self, width: u32, height: u32) -> wgpu::SurfaceConfiguration {
        let usage = if self.copy_src {
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC
        } else {
            wgpu::TextureUsages::RENDER_ATTACHMENT
        };
        wgpu::SurfaceConfiguration {
            usage,
            format: self.format,
            color_space: self.color_space,
            width,
            height,
            present_mode: self.present_mode,
            desired_maximum_frame_latency: self.frame_latency,
            alpha_mode: self.alpha_mode,
            view_formats: Vec::new(),
        }
    }
}

/// Plans a window swapchain. `None` when the surface lists no format at all.
#[must_use]
pub fn plan_swapchain(caps: &wgpu::SurfaceCapabilities, request: &SwapchainRequest) -> Option<SwapchainPlan> {
    // SDR keeps the historical choice: the first sRGB format (wgpu lists them first), `Auto`.
    let sdr_format = caps
        .formats
        .iter()
        .copied()
        .find(wgpu::TextureFormat::is_srgb)
        .or_else(|| caps.formats.first().copied())?;
    let (format, color_space, hdr_fallback) = match request.hdr {
        HdrOutput::Sdr => (sdr_format, wgpu::SurfaceColorSpace::Auto, None),
        _ if request.display_hdr == Some(false) => (
            sdr_format,
            wgpu::SurfaceColorSpace::Auto,
            Some(HdrFallback::DisplaySdr),
        ),
        hdr => {
            let choice = select_surface_format(&caps.formats, |f| caps.color_spaces(f), hdr);
            if choice.hdr {
                (choice.format, choice.color_space, None)
            } else {
                (
                    sdr_format,
                    wgpu::SurfaceColorSpace::Auto,
                    Some(HdrFallback::NotOffered),
                )
            }
        }
    };
    let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
        wgpu::CompositeAlphaMode::Opaque
    } else {
        caps.alpha_modes
            .first()
            .copied()
            .unwrap_or(wgpu::CompositeAlphaMode::Auto)
    };
    Some(SwapchainPlan {
        format,
        color_space,
        present_mode: plan_present_mode(request, &caps.present_modes),
        alpha_mode,
        frame_latency: FRAME_LATENCY,
        copy_src: caps.usages.contains(wgpu::TextureUsages::COPY_SRC),
        hdr_active: request.hdr != HdrOutput::Sdr && hdr_fallback.is_none(),
        hdr_fallback,
    })
}

/// The present mode for `request` from the modes the surface listed.
///
/// [`PresentPolicy::Vsync`] keeps the window's mapping (`Vsync` → `AutoVsync`, `VsyncRelaxed` →
/// `FifoRelaxed` when listed, `Immediate` → `AutoNoVsync`). VRR on DX12 takes `Immediate` when
/// listed (the module docs say why). Everything else is [`select_present_mode`].
#[must_use]
pub fn plan_present_mode(request: &SwapchainRequest, supported: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    use regia_platform::PresentMode as Window;
    match request.present {
        PresentPolicy::Vsync => match request.window_present {
            Window::Vsync => wgpu::PresentMode::AutoVsync,
            Window::VsyncRelaxed if supported.contains(&wgpu::PresentMode::FifoRelaxed) => {
                wgpu::PresentMode::FifoRelaxed
            }
            Window::VsyncRelaxed => wgpu::PresentMode::AutoVsync,
            Window::Immediate => wgpu::PresentMode::AutoNoVsync,
        },
        PresentPolicy::Vrr
            if request.backend == wgpu::Backend::Dx12
                && supported.contains(&wgpu::PresentMode::Immediate) =>
        {
            wgpu::PresentMode::Immediate
        }
        policy => select_present_mode(policy, supported),
    }
}

/// The concrete mode wgpu configures for `mode`, by wgpu's documented resolution of the `Auto`
/// modes: `AutoVsync` is `FifoRelaxed`, else `Fifo`; `AutoNoVsync` is `Immediate`, else `Mailbox`,
/// else `Fifo`. A concrete mode is returned as is.
#[must_use]
pub fn effective_present_mode(mode: wgpu::PresentMode, supported: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    let first = |order: &[wgpu::PresentMode]| {
        order
            .iter()
            .copied()
            .find(|m| supported.contains(m))
            .unwrap_or(wgpu::PresentMode::Fifo)
    };
    match mode {
        wgpu::PresentMode::AutoVsync => first(&[wgpu::PresentMode::FifoRelaxed, wgpu::PresentMode::Fifo]),
        wgpu::PresentMode::AutoNoVsync => first(&[
            wgpu::PresentMode::Immediate,
            wgpu::PresentMode::Mailbox,
            wgpu::PresentMode::Fifo,
        ]),
        concrete => concrete,
    }
}

/// Whether this present asks for the compositor's frame callback (winit `pre_present_notify`, which only
/// Wayland implements). While one is pending winit holds the next `RedrawRequested`, and the compositor answers
/// after it has shown the frame. So with the callback, the next frame's CPU work cannot start while the GPU
/// renders this one: a frame costs CPU + GPU, and one that fits a vblank on each side alone still misses it.
/// On the Deck OLED that held the `Deck` preset at 30 fps at every cap; without the callback it holds 90
/// (ADR-1750, `docs/evidence/deck/2026-10-29/summary.md`).
///
/// A focused window skips it on every present mode, as on X11 and Windows. On a vsync mode (`Fifo`,
/// `FifoRelaxed`) the swapchain paces the loop by itself. With vsync off (`Immediate`, `Mailbox`) nothing should
/// pace it but the frame cap: a compositor that throttles frame callbacks otherwise holds every frame until the
/// GPU finished the last one (ADR-8170, which amends ADR-1750 (5)). Every unfocused window keeps it, because the
/// runner detects a hidden workspace by the missing callback.
///
/// Every present (the frame, the frame-generation present, the boot splash) asks this before it calls
/// `pre_present_notify`.
#[must_use]
pub fn wants_frame_callback(focused: bool, effective: wgpu::PresentMode) -> bool {
    use wgpu::PresentMode::{AutoNoVsync, AutoVsync, Fifo, FifoRelaxed, Immediate, Mailbox};
    // Exhaustive on purpose: a new wgpu mode must decide whether the compositor should pace it.
    let compositor_paces_focused = match effective {
        // The swapchain waits for the vblank.
        Fifo | FifoRelaxed | AutoVsync => false,
        // Vsync off: the frame cap (or nothing) paces the loop.
        Immediate | Mailbox | AutoNoVsync => false,
    };
    !focused || compositor_paces_focused
}

/// The HDR outputs the surface lists (HDR10, scRGB), in that order.
#[must_use]
pub fn offered_hdr_outputs(caps: &wgpu::SurfaceCapabilities) -> Vec<HdrOutput> {
    [HdrOutput::Hdr10, HdrOutput::ScRgb]
        .into_iter()
        .filter(|&hdr| select_surface_format(&caps.formats, |f| caps.color_spaces(f), hdr).hdr)
        .collect()
}

/// What the display reports about HDR (wgpu `Surface::display_hdr_info`; DXGI
/// `IDXGIOutput6::GetDesc1` on Windows, for DX12 and Vulkan alike). Every field is advisory and
/// `None` when the platform does not say.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DisplayHdrSummary {
    /// The display is in an HDR mode (`Some(false)`: SDR, Windows HDR off).
    pub hdr_mode: Option<bool>,
    /// Peak luminance of a small patch, cd/m².
    pub peak_nits: Option<f32>,
    /// Sustained full-frame luminance, cd/m².
    pub full_frame_nits: Option<f32>,
    /// Black level, cd/m².
    pub min_nits: Option<f32>,
    /// Luminance the OS maps SDR white to, cd/m² (the Windows "SDR content brightness" slider).
    pub sdr_white_nits: Option<f32>,
    /// Output signal bits per color channel.
    pub bits_per_color: Option<u8>,
}

impl DisplayHdrSummary {
    /// Reads wgpu's report. A non-finite value, or a zero or negative peak, reads as unknown.
    #[must_use]
    pub fn from_info(info: &wgpu::DisplayHdrInfo) -> Self {
        let positive = |v: Option<f32>| v.filter(|v| v.is_finite() && *v > 0.0);
        let luminance = info.luminance.unwrap_or_default();
        Self {
            hdr_mode: info.coarse.and_then(|c| c.high_dynamic_range),
            peak_nits: positive(luminance.max_nits),
            full_frame_nits: positive(luminance.max_full_frame_nits),
            min_nits: luminance.min_nits.filter(|v| v.is_finite() && *v >= 0.0),
            sdr_white_nits: positive(luminance.sdr_white_nits),
            bits_per_color: info.bits_per_color,
        }
    }

    /// The display's luminance range in the calibration's terms ([`DisplayLuminance`]).
    #[must_use]
    pub fn luminance(&self) -> DisplayLuminance {
        DisplayLuminance {
            peak_nits: self.peak_nits,
            min_nits: self.min_nits,
        }
    }
}

/// What a live window surface was configured with, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct SwapchainInfo {
    /// The inputs of the plan.
    pub request: SwapchainRequest,
    /// The plan in effect.
    pub plan: SwapchainPlan,
    /// Physical size of the swapchain.
    pub size: (u32, u32),
    /// Present modes the surface lists.
    pub offered_present_modes: Vec<wgpu::PresentMode>,
    /// HDR outputs the surface lists.
    pub offered_hdr: Vec<HdrOutput>,
    /// What the display reported when the plan was made.
    pub display: DisplayHdrSummary,
}

impl SwapchainInfo {
    /// Plans a `size` swapchain ([`plan_swapchain`]) and keeps what the plan was made from. The
    /// request's `display_hdr` is taken from `display`. `None` when the surface lists no format.
    #[must_use]
    pub fn plan(
        caps: &wgpu::SurfaceCapabilities,
        request: SwapchainRequest,
        display: DisplayHdrSummary,
        size: (u32, u32),
    ) -> Option<Self> {
        let request = SwapchainRequest {
            display_hdr: display.hdr_mode,
            ..request
        };
        Some(Self {
            plan: plan_swapchain(caps, &request)?,
            request,
            size,
            offered_present_modes: caps.present_modes.clone(),
            offered_hdr: offered_hdr_outputs(caps),
            display,
        })
    }

    /// The concrete present mode wgpu runs ([`effective_present_mode`]).
    #[must_use]
    pub fn effective_present_mode(&self) -> wgpu::PresentMode {
        effective_present_mode(self.plan.present_mode, &self.offered_present_modes)
    }

    /// The present modes of this swapchain by name, for a host without a wgpu dependency
    /// (ADR-7880).
    #[must_use]
    pub fn present_summary(&self) -> PresentSummary {
        let effective = self.effective_present_mode();
        let has = |mode| self.offered_present_modes.contains(&mode);
        PresentSummary {
            planned: present_mode_name(self.plan.present_mode),
            effective: present_mode_name(effective),
            waits_for_vblank: matches!(
                effective,
                wgpu::PresentMode::Fifo | wgpu::PresentMode::FifoRelaxed
            ),
            offers_immediate: has(wgpu::PresentMode::Immediate),
            offers_mailbox: has(wgpu::PresentMode::Mailbox),
        }
    }
}

/// A window's present mode in plain names (ADR-7880): what a settings menu or the agent reports
/// after a vsync change, without naming wgpu types. [`crate::WindowSurfaces::present_summary`]
/// reads it from a live swapchain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentSummary {
    /// The mode handed to wgpu ([`present_mode_name`]: `AutoVsync`, `AutoNoVsync`, `Fifo`, ...).
    pub planned: &'static str,
    /// The mode wgpu configures: the `Auto` modes resolved against the modes the surface offers
    /// ([`effective_present_mode`]).
    pub effective: &'static str,
    /// The effective mode waits for vertical blank (`Fifo` or `FifoRelaxed`): vsync is on.
    pub waits_for_vblank: bool,
    /// The surface offers `Immediate` (tearing, no wait).
    pub offers_immediate: bool,
    /// The surface offers `Mailbox` (no tearing, no wait).
    pub offers_mailbox: bool,
}

/// The wgpu name of a present mode (`Fifo`, `FifoRelaxed`, `Immediate`, `Mailbox`, `AutoVsync`,
/// `AutoNoVsync`), as `render.swapchain` prints them.
#[must_use]
pub fn present_mode_name(mode: wgpu::PresentMode) -> &'static str {
    match mode {
        wgpu::PresentMode::AutoVsync => "AutoVsync",
        wgpu::PresentMode::AutoNoVsync => "AutoNoVsync",
        wgpu::PresentMode::Fifo => "Fifo",
        wgpu::PresentMode::FifoRelaxed => "FifoRelaxed",
        wgpu::PresentMode::Immediate => "Immediate",
        wgpu::PresentMode::Mailbox => "Mailbox",
    }
}

/// The calibration's display luminance after a display report: the platform's values win, the
/// report fills what the platform left unknown. `None` when nothing changes.
#[must_use]
pub fn merge_display_luminance(
    platform: Option<DisplayLuminance>,
    display: &DisplayHdrSummary,
) -> Option<DisplayLuminance> {
    let current = platform.unwrap_or_default();
    let merged = DisplayLuminance {
        peak_nits: current.peak_nits.or(display.peak_nits),
        min_nits: current.min_nits.or(display.min_nits),
    };
    (platform != Some(merged) && merged != DisplayLuminance::default()).then_some(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(
        formats: &[(wgpu::TextureFormat, wgpu::SurfaceColorSpaces)],
        present_modes: &[wgpu::PresentMode],
        alpha_modes: &[wgpu::CompositeAlphaMode],
    ) -> wgpu::SurfaceCapabilities {
        wgpu::SurfaceCapabilities {
            // What wgpu-core does: sRGB formats first, and `formats` holds those `Auto` resolves.
            formats: {
                let mut list: Vec<_> = formats
                    .iter()
                    .filter(|(f, spaces)| {
                        spaces.contains(wgpu::SurfaceColorSpaces::SRGB)
                            || (*f == wgpu::TextureFormat::Rgba16Float
                                && spaces.contains(wgpu::SurfaceColorSpaces::EXTENDED_SRGB_LINEAR))
                    })
                    .map(|(f, _)| *f)
                    .collect();
                list.sort_by_key(|f| !f.is_srgb());
                list
            },
            format_capabilities: formats
                .iter()
                .map(|&(format, color_spaces)| wgpu::SurfaceFormatCapabilities { format, color_spaces })
                .collect(),
            present_modes: present_modes.to_vec(),
            alpha_modes: alpha_modes.to_vec(),
            usages: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        }
    }

    /// The DX12 surface of a window, as wgpu-hal 30 reports it (formats, color spaces, modes;
    /// `Immediate` only with tearing support; `Opaque` only for a window handle).
    fn dx12_caps(tearing: bool) -> wgpu::SurfaceCapabilities {
        let mut modes = vec![wgpu::PresentMode::Mailbox, wgpu::PresentMode::Fifo];
        if tearing {
            modes.push(wgpu::PresentMode::Immediate);
        }
        caps(
            &[
                (
                    wgpu::TextureFormat::Bgra8UnormSrgb,
                    wgpu::SurfaceColorSpaces::SRGB,
                ),
                (wgpu::TextureFormat::Bgra8Unorm, wgpu::SurfaceColorSpaces::SRGB),
                (
                    wgpu::TextureFormat::Rgba8UnormSrgb,
                    wgpu::SurfaceColorSpaces::SRGB,
                ),
                (wgpu::TextureFormat::Rgba8Unorm, wgpu::SurfaceColorSpaces::SRGB),
                (
                    wgpu::TextureFormat::Rgb10a2Unorm,
                    wgpu::SurfaceColorSpaces::SRGB | wgpu::SurfaceColorSpaces::BT2100_PQ,
                ),
                (
                    wgpu::TextureFormat::Rgba16Float,
                    wgpu::SurfaceColorSpaces::EXTENDED_SRGB_LINEAR,
                ),
            ],
            &modes,
            &[wgpu::CompositeAlphaMode::Opaque],
        )
    }

    /// A Vulkan surface (Linux desktop): sRGB only, every present mode, two alpha modes.
    fn vulkan_caps() -> wgpu::SurfaceCapabilities {
        caps(
            &[
                (
                    wgpu::TextureFormat::Bgra8UnormSrgb,
                    wgpu::SurfaceColorSpaces::SRGB,
                ),
                (wgpu::TextureFormat::Bgra8Unorm, wgpu::SurfaceColorSpaces::SRGB),
            ],
            &[
                wgpu::PresentMode::Fifo,
                wgpu::PresentMode::FifoRelaxed,
                wgpu::PresentMode::Immediate,
                wgpu::PresentMode::Mailbox,
            ],
            &[
                wgpu::CompositeAlphaMode::PreMultiplied,
                wgpu::CompositeAlphaMode::Opaque,
            ],
        )
    }

    fn request(backend: wgpu::Backend, present: PresentPolicy, hdr: HdrOutput) -> SwapchainRequest {
        SwapchainRequest {
            backend,
            present,
            window_present: regia_platform::PresentMode::Vsync,
            hdr,
            display_hdr: None,
        }
    }

    #[test]
    fn the_default_request_keeps_the_historical_sdr_configuration() {
        for (backend, caps) in [
            (wgpu::Backend::Dx12, dx12_caps(true)),
            (wgpu::Backend::Vulkan, vulkan_caps()),
        ] {
            let plan = plan_swapchain(&caps, &request(backend, PresentPolicy::Vsync, HdrOutput::Sdr))
                .expect("formats listed"); // invariant: test fixture lists formats
            assert_eq!(plan.format, wgpu::TextureFormat::Bgra8UnormSrgb, "{backend:?}");
            assert_eq!(plan.color_space, wgpu::SurfaceColorSpace::Auto);
            assert_eq!(plan.present_mode, wgpu::PresentMode::AutoVsync);
            assert_eq!(plan.frame_latency, FRAME_LATENCY);
            assert!(plan.copy_src);
            assert!(!plan.hdr_active);
            assert_eq!(plan.hdr_fallback, None);
            let config = plan.configuration(1920, 1080);
            assert_eq!((config.width, config.height), (1920, 1080));
            assert_eq!(config.desired_maximum_frame_latency, 2);
            assert!(config.usage.contains(wgpu::TextureUsages::COPY_SRC));
            assert!(config.view_formats.is_empty());
        }
    }

    #[test]
    fn alpha_is_opaque_whenever_the_surface_lists_it() {
        let vk = plan_swapchain(
            &vulkan_caps(),
            &request(wgpu::Backend::Vulkan, PresentPolicy::Vsync, HdrOutput::Sdr),
        );
        assert_eq!(
            vk.map(|p| p.alpha_mode),
            Some(wgpu::CompositeAlphaMode::Opaque),
            "Opaque beats a premultiplied first entry"
        );
        let mut inherit_only = vulkan_caps();
        inherit_only.alpha_modes = vec![wgpu::CompositeAlphaMode::Inherit];
        let plan = plan_swapchain(
            &inherit_only,
            &request(wgpu::Backend::Vulkan, PresentPolicy::Vsync, HdrOutput::Sdr),
        );
        assert_eq!(
            plan.map(|p| p.alpha_mode),
            Some(wgpu::CompositeAlphaMode::Inherit)
        );
        let mut none = vulkan_caps();
        none.alpha_modes.clear();
        let plan = plan_swapchain(
            &none,
            &request(wgpu::Backend::Vulkan, PresentPolicy::Vsync, HdrOutput::Sdr),
        );
        assert_eq!(plan.map(|p| p.alpha_mode), Some(wgpu::CompositeAlphaMode::Auto));
    }

    #[test]
    fn dx12_hdr10_and_scrgb_use_their_documented_pairs() {
        let caps = dx12_caps(true);
        assert_eq!(
            offered_hdr_outputs(&caps),
            vec![HdrOutput::Hdr10, HdrOutput::ScRgb]
        );
        let hdr10 = plan_swapchain(
            &caps,
            &request(wgpu::Backend::Dx12, PresentPolicy::Vsync, HdrOutput::Hdr10),
        )
        .expect("formats listed"); // invariant: test fixture lists formats
        assert_eq!(hdr10.format, wgpu::TextureFormat::Rgb10a2Unorm);
        assert_eq!(hdr10.color_space, wgpu::SurfaceColorSpace::Bt2100Pq);
        assert!(hdr10.hdr_active);
        let scrgb = plan_swapchain(
            &caps,
            &request(wgpu::Backend::Dx12, PresentPolicy::Vsync, HdrOutput::ScRgb),
        )
        .expect("formats listed"); // invariant: test fixture lists formats
        assert_eq!(scrgb.format, wgpu::TextureFormat::Rgba16Float);
        assert_eq!(scrgb.color_space, wgpu::SurfaceColorSpace::ExtendedSrgbLinear);
        assert!(scrgb.hdr_active);
        assert_eq!(scrgb.hdr_fallback, None);
    }

    #[test]
    fn an_sdr_display_turns_an_hdr_request_into_sdr() {
        let caps = dx12_caps(true);
        for hdr in [HdrOutput::Hdr10, HdrOutput::ScRgb] {
            let mut req = request(wgpu::Backend::Dx12, PresentPolicy::Vsync, hdr);
            req.display_hdr = Some(false);
            let plan = plan_swapchain(&caps, &req).expect("formats listed"); // invariant: test fixture lists formats
            assert_eq!(plan.format, wgpu::TextureFormat::Bgra8UnormSrgb, "{hdr:?}");
            assert_eq!(plan.color_space, wgpu::SurfaceColorSpace::Auto);
            assert!(!plan.hdr_active);
            assert_eq!(plan.hdr_fallback, Some(HdrFallback::DisplaySdr));
            assert_eq!(HdrFallback::DisplaySdr.code(), "display_sdr");
            // HDR mode on, or unknown: the request stands.
            for display_hdr in [Some(true), None] {
                req.display_hdr = display_hdr;
                let plan = plan_swapchain(&caps, &req).expect("formats listed"); // invariant: test fixture lists formats
                assert!(plan.hdr_active, "{hdr:?} {display_hdr:?}");
            }
        }
        // An SDR request on an SDR display is plain SDR, not a fallback.
        let mut req = request(wgpu::Backend::Dx12, PresentPolicy::Vsync, HdrOutput::Sdr);
        req.display_hdr = Some(false);
        assert_eq!(plan_swapchain(&caps, &req).and_then(|p| p.hdr_fallback), None);
    }

    #[test]
    fn hdr_not_listed_falls_back_with_a_reason() {
        let plan = plan_swapchain(
            &vulkan_caps(),
            &request(wgpu::Backend::Vulkan, PresentPolicy::Vsync, HdrOutput::Hdr10),
        )
        .expect("formats listed"); // invariant: test fixture lists formats
        assert!(!plan.hdr_active);
        assert_eq!(plan.format, wgpu::TextureFormat::Bgra8UnormSrgb);
        assert_eq!(plan.color_space, wgpu::SurfaceColorSpace::Auto);
        assert_eq!(plan.hdr_fallback, Some(HdrFallback::NotOffered));
        assert_eq!(HdrFallback::NotOffered.code(), "not_offered");
        assert!(offered_hdr_outputs(&vulkan_caps()).is_empty());
    }

    /// A surface that lists `Rgb10a2Unorm` with PQ only (no sRGB) leaves it out of `formats`;
    /// HDR10 must still be found through the per-format color spaces.
    #[test]
    fn an_hdr_only_format_is_found_outside_the_auto_format_list() {
        let caps = caps(
            &[
                (
                    wgpu::TextureFormat::Bgra8UnormSrgb,
                    wgpu::SurfaceColorSpaces::SRGB,
                ),
                (
                    wgpu::TextureFormat::Rgb10a2Unorm,
                    wgpu::SurfaceColorSpaces::BT2100_PQ,
                ),
            ],
            &[wgpu::PresentMode::Fifo],
            &[wgpu::CompositeAlphaMode::Opaque],
        );
        assert!(!caps.formats.contains(&wgpu::TextureFormat::Rgb10a2Unorm));
        let plan = plan_swapchain(
            &caps,
            &request(wgpu::Backend::Vulkan, PresentPolicy::Vsync, HdrOutput::Hdr10),
        )
        .expect("formats listed"); // invariant: test fixture lists formats
        assert!(plan.hdr_active);
        assert_eq!(plan.format, wgpu::TextureFormat::Rgb10a2Unorm);
        assert_eq!(plan.color_space, wgpu::SurfaceColorSpace::Bt2100Pq);
    }

    #[test]
    fn vrr_on_dx12_presents_with_tearing_allowed_when_it_can() {
        let vrr = |backend, caps: &wgpu::SurfaceCapabilities| {
            plan_present_mode(
                &request(backend, PresentPolicy::Vrr, HdrOutput::Sdr),
                &caps.present_modes,
            )
        };
        assert_eq!(
            vrr(wgpu::Backend::Dx12, &dx12_caps(true)),
            wgpu::PresentMode::Immediate
        );
        assert_eq!(
            vrr(wgpu::Backend::Dx12, &dx12_caps(false)),
            wgpu::PresentMode::Mailbox
        );
        // Vulkan keeps the mailbox-first rule, also on Windows.
        assert_eq!(
            vrr(wgpu::Backend::Vulkan, &vulkan_caps()),
            wgpu::PresentMode::Mailbox
        );
        assert_eq!(
            vrr(wgpu::Backend::Vulkan, &dx12_caps(true)),
            wgpu::PresentMode::Mailbox
        );
    }

    /// Every (present mode, focused) pair (ADR-1750 (5), amended by ADR-8170). A focused window skips the
    /// callback on every mode: FIFO paces the loop by itself, and with vsync off (Immediate, Mailbox) nothing
    /// should pace it, so a compositor that throttles frame callbacks must not serialize CPU and GPU work. An
    /// unfocused window keeps it on every mode: it may be on a hidden workspace, and the runner finds that
    /// by the missing callback.
    #[test]
    fn frame_callback_for_every_present_mode_and_focus() {
        use wgpu::PresentMode::{AutoNoVsync, AutoVsync, Fifo, FifoRelaxed, Immediate, Mailbox};
        let table = [
            // (mode, focused, wants the callback)
            (Fifo, true, false),
            (FifoRelaxed, true, false),
            (Immediate, true, false),
            (Mailbox, true, false),
            (AutoVsync, true, false),
            (AutoNoVsync, true, false),
            (Fifo, false, true),
            (FifoRelaxed, false, true),
            (Immediate, false, true),
            (Mailbox, false, true),
            (AutoVsync, false, true),
            (AutoNoVsync, false, true),
        ];
        let mut wrong = Vec::new();
        for (mode, focused, want) in table {
            let got = wants_frame_callback(focused, mode);
            if got != want {
                wrong.push(format!("{mode:?} focused={focused}: got {got}, want {want}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "wants_frame_callback disagrees with ADR-8170: {wrong:#?}"
        );
        // The table covers every mode wgpu has, focused and not.
        assert_eq!(table.len(), 6 * 2);
    }

    /// Every `pre_present_notify` call in this crate's source asks `wants_frame_callback` first (ADR-8170: the
    /// boot splash's present called it unconditionally). A call counts as asked when the check appears in the
    /// lines just above it, which is how every present writes it.
    #[test]
    fn every_present_in_the_crate_asks_wants_frame_callback() {
        fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let entries = std::fs::read_dir(dir).expect("source dir readable"); // invariant: the crate's own src
            for entry in entries {
                let path = entry.expect("dir entry").path(); // invariant: the crate's own src
                if path.is_dir() {
                    rust_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        files.sort();
        let needle = ["pre_present", "_notify()"].concat();
        let check = ["wants_frame", "_callback("].concat();
        let mut calls = 0;
        let mut unguarded = Vec::new();
        for file in &files {
            let text = std::fs::read_to_string(file).expect("source file readable"); // invariant: listed above
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !line.contains(&needle) || line.trim_start().starts_with("//") {
                    continue;
                }
                calls += 1;
                let from = i.saturating_sub(8);
                if !lines[from..i].iter().any(|l| l.contains(&check)) {
                    unguarded.push(format!("{}:{}", file.display(), i + 1));
                }
            }
        }
        assert!(
            unguarded.is_empty(),
            "pre_present_notify without wants_frame_callback: {unguarded:#?}"
        );
        // The frame, the frame-generation present and the boot splash.
        assert!(
            calls >= 3,
            "found {calls} pre_present_notify calls; the scan lost the crate's presents"
        );
    }

    /// The vsync-off modes `effective_present_mode` resolves `AutoNoVsync` to skip the callback when focused,
    /// whatever the surface offers (the DO27 Hyprland case: vsync off is `Immediate`).
    #[test]
    fn a_focused_vsync_off_window_skips_the_frame_callback_on_every_surface() {
        use wgpu::PresentMode::{AutoNoVsync, AutoVsync, Fifo, FifoRelaxed, Immediate, Mailbox};
        let offers: [&[wgpu::PresentMode]; 4] = [
            &[Fifo],
            &[Fifo, Mailbox],
            &[Fifo, Immediate],
            &[Fifo, FifoRelaxed, Immediate, Mailbox],
        ];
        for offered in offers {
            for requested in [AutoVsync, AutoNoVsync, Fifo, FifoRelaxed, Immediate, Mailbox] {
                let effective = effective_present_mode(requested, offered);
                assert!(
                    !wants_frame_callback(true, effective),
                    "{requested:?} on {offered:?} -> {effective:?}: a focused window must skip the callback"
                );
                assert!(
                    wants_frame_callback(false, effective),
                    "{requested:?} on {offered:?} -> {effective:?}: an unfocused window keeps the callback"
                );
            }
        }
    }

    #[test]
    fn present_modes_per_policy_and_window_mode() {
        use regia_platform::PresentMode as Window;
        let dx12 = dx12_caps(true);
        let vk = vulkan_caps();
        let mut req = request(wgpu::Backend::Dx12, PresentPolicy::Uncapped, HdrOutput::Sdr);
        assert_eq!(
            plan_present_mode(&req, &dx12.present_modes),
            wgpu::PresentMode::Immediate
        );
        assert_eq!(
            plan_present_mode(&req, &dx12_caps(false).present_modes),
            wgpu::PresentMode::AutoNoVsync
        );
        req.present = PresentPolicy::Vsync;
        for (window, on_dx12, on_vk) in [
            (
                Window::Vsync,
                wgpu::PresentMode::AutoVsync,
                wgpu::PresentMode::AutoVsync,
            ),
            (
                Window::VsyncRelaxed,
                wgpu::PresentMode::AutoVsync,
                wgpu::PresentMode::FifoRelaxed,
            ),
            (
                Window::Immediate,
                wgpu::PresentMode::AutoNoVsync,
                wgpu::PresentMode::AutoNoVsync,
            ),
        ] {
            req.window_present = window;
            assert_eq!(
                plan_present_mode(&req, &dx12.present_modes),
                on_dx12,
                "{window:?}"
            );
            assert_eq!(plan_present_mode(&req, &vk.present_modes), on_vk, "{window:?}");
        }
    }

    #[test]
    fn auto_modes_resolve_like_wgpu() {
        let dx12 = dx12_caps(true).present_modes;
        let dx12_no_tearing = dx12_caps(false).present_modes;
        let vk = vulkan_caps().present_modes;
        // DX12 has no FifoRelaxed: vsync is FIFO.
        assert_eq!(
            effective_present_mode(wgpu::PresentMode::AutoVsync, &dx12),
            wgpu::PresentMode::Fifo
        );
        assert_eq!(
            effective_present_mode(wgpu::PresentMode::AutoVsync, &vk),
            wgpu::PresentMode::FifoRelaxed
        );
        assert_eq!(
            effective_present_mode(wgpu::PresentMode::AutoNoVsync, &dx12),
            wgpu::PresentMode::Immediate
        );
        assert_eq!(
            effective_present_mode(wgpu::PresentMode::AutoNoVsync, &dx12_no_tearing),
            wgpu::PresentMode::Mailbox
        );
        assert_eq!(
            effective_present_mode(wgpu::PresentMode::AutoNoVsync, &[]),
            wgpu::PresentMode::Fifo
        );
        assert_eq!(
            effective_present_mode(wgpu::PresentMode::Mailbox, &[]),
            wgpu::PresentMode::Mailbox
        );
    }

    #[test]
    fn the_info_keeps_the_offer_and_takes_the_display_state() {
        let display = DisplayHdrSummary {
            hdr_mode: Some(false),
            peak_nits: Some(300.0),
            ..Default::default()
        };
        let info = SwapchainInfo::plan(
            &dx12_caps(true),
            request(wgpu::Backend::Dx12, PresentPolicy::Uncapped, HdrOutput::Hdr10),
            display,
            (2560, 1440),
        )
        .expect("formats listed"); // invariant: test fixture lists formats
        assert_eq!(
            info.request.display_hdr,
            Some(false),
            "taken from the display report"
        );
        assert_eq!(info.plan.hdr_fallback, Some(HdrFallback::DisplaySdr));
        assert_eq!(info.size, (2560, 1440));
        assert_eq!(info.offered_hdr, vec![HdrOutput::Hdr10, HdrOutput::ScRgb]);
        assert_eq!(info.offered_present_modes, dx12_caps(true).present_modes);
        assert_eq!(info.plan.present_mode, wgpu::PresentMode::Immediate);
        assert_eq!(info.effective_present_mode(), wgpu::PresentMode::Immediate);
        assert_eq!(info.display, display);
    }

    #[test]
    fn no_format_means_no_plan() {
        let empty = wgpu::SurfaceCapabilities::default();
        assert_eq!(
            plan_swapchain(
                &empty,
                &request(wgpu::Backend::Dx12, PresentPolicy::Vsync, HdrOutput::Sdr)
            ),
            None
        );
    }

    #[test]
    fn display_reports_read_as_advisory_values() {
        let info = wgpu::DisplayHdrInfo {
            luminance: Some(wgpu::DisplayLuminance {
                max_nits: Some(1015.0),
                max_full_frame_nits: Some(f32::NAN),
                min_nits: Some(0.05),
                sdr_white_nits: Some(240.0),
            }),
            coarse: Some(wgpu::DisplayCoarseRange {
                high_dynamic_range: Some(true),
                gamut: None,
            }),
            bits_per_color: Some(10),
            ..Default::default()
        };
        let summary = DisplayHdrSummary::from_info(&info);
        assert_eq!(summary.hdr_mode, Some(true));
        assert_eq!(summary.peak_nits, Some(1015.0));
        assert_eq!(summary.full_frame_nits, None, "NaN reads as unknown");
        assert_eq!(summary.min_nits, Some(0.05));
        assert_eq!(summary.sdr_white_nits, Some(240.0));
        assert_eq!(summary.bits_per_color, Some(10));
        assert_eq!(
            summary.luminance(),
            DisplayLuminance {
                peak_nits: Some(1015.0),
                min_nits: Some(0.05)
            }
        );
        let zero_peak = wgpu::DisplayHdrInfo {
            luminance: Some(wgpu::DisplayLuminance {
                max_nits: Some(0.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(DisplayHdrSummary::from_info(&zero_peak).peak_nits, None);
        assert_eq!(
            DisplayHdrSummary::from_info(&wgpu::DisplayHdrInfo::default()),
            DisplayHdrSummary::default()
        );
    }

    #[test]
    fn the_platform_luminance_wins_and_the_display_fills_gaps() {
        let display = DisplayHdrSummary {
            peak_nits: Some(800.0),
            min_nits: Some(0.1),
            ..Default::default()
        };
        // Desktop platforms report nothing: the display report fills both.
        assert_eq!(
            merge_display_luminance(Some(DisplayLuminance::default()), &display),
            Some(DisplayLuminance {
                peak_nits: Some(800.0),
                min_nits: Some(0.1)
            })
        );
        assert_eq!(merge_display_luminance(None, &display), Some(display.luminance()));
        // Android reports its own: unchanged.
        let android = DisplayLuminance {
            peak_nits: Some(600.0),
            min_nits: Some(0.2),
        };
        assert_eq!(merge_display_luminance(Some(android), &display), None);
        // Nothing reported anywhere: nothing to insert.
        assert_eq!(merge_display_luminance(None, &DisplayHdrSummary::default()), None);
    }

    /// ADR-7880: a vsync setting switched off is the window's `Immediate` present mode. The
    /// summary reports what the surface actually runs: `Immediate` when offered, else `Mailbox`,
    /// else `Fifo` (vsync stays on, `waits_for_vblank`). Switching back is `AutoVsync` (FIFO).
    #[test]
    fn a_vsync_toggle_reports_the_mode_the_surface_runs() {
        let plan = |window_present, modes: &[wgpu::PresentMode]| {
            let caps = caps(
                &[(
                    wgpu::TextureFormat::Bgra8UnormSrgb,
                    wgpu::SurfaceColorSpaces::SRGB,
                )],
                modes,
                &[wgpu::CompositeAlphaMode::Opaque],
            );
            let request = SwapchainRequest {
                backend: wgpu::Backend::Vulkan,
                present: PresentPolicy::Vsync,
                window_present,
                hdr: HdrOutput::Sdr,
                display_hdr: None,
            };
            SwapchainInfo::plan(&caps, request, DisplayHdrSummary::default(), (1280, 720))
                .expect("a format is offered")
                .present_summary()
        };
        use regia_platform::PresentMode as Window;
        let all = [
            wgpu::PresentMode::Fifo,
            wgpu::PresentMode::Mailbox,
            wgpu::PresentMode::Immediate,
        ];
        let off = plan(Window::Immediate, &all);
        assert_eq!((off.planned, off.effective), ("AutoNoVsync", "Immediate"));
        assert!(!off.waits_for_vblank && off.offers_immediate && off.offers_mailbox);
        let on = plan(Window::Vsync, &all);
        assert_eq!((on.planned, on.effective), ("AutoVsync", "Fifo"));
        assert!(on.waits_for_vblank);
        // No tearing mode: Mailbox (Android, Wayland compositors without tearing control).
        let mailbox = plan(
            Window::Immediate,
            &[wgpu::PresentMode::Fifo, wgpu::PresentMode::Mailbox],
        );
        assert_eq!(mailbox.effective, "Mailbox");
        assert!(!mailbox.waits_for_vblank && !mailbox.offers_immediate);
        // FIFO only: the fallback keeps vsync on, and the summary says so.
        let fifo = plan(Window::Immediate, &[wgpu::PresentMode::Fifo]);
        assert_eq!((fifo.planned, fifo.effective), ("AutoNoVsync", "Fifo"));
        assert!(fifo.waits_for_vblank);
    }

    #[test]
    fn present_mode_names_match_the_swapchain_report() {
        for mode in [
            wgpu::PresentMode::AutoVsync,
            wgpu::PresentMode::AutoNoVsync,
            wgpu::PresentMode::Fifo,
            wgpu::PresentMode::FifoRelaxed,
            wgpu::PresentMode::Immediate,
            wgpu::PresentMode::Mailbox,
        ] {
            // `render.swapchain` prints `{:?}`; the summary must use the same names.
            assert_eq!(present_mode_name(mode), format!("{mode:?}"));
        }
    }
}
