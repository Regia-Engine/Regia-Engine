// SPDX-License-Identifier: MPL-2.0
//! Display output: present-mode selection, horizontal-plus FOV, HDR10/scRGB (`specs/26` § 4).
//!
//! wgpu 30 exposes swapchain colour spaces ([`wgpu::SurfaceColorSpace::Bt2100Pq`] for HDR10 and
//! [`wgpu::SurfaceColorSpace::ExtendedSrgbLinear`] for scRGB). Regia Engine selects one when the surface
//! lists it and falls back to SDR sRGB otherwise (ADR-021). ST 2086 mastering metadata is not set:
//! wgpu does not expose it.

use regia_app::prelude::*;

use crate::profile::RenderProfile;

/// Reference aspect for horizontal-plus FOV (16:9).
pub const HORIZONTAL_PLUS_REFERENCE_ASPECT: f32 = 16.0 / 9.0;

/// How the swapchain should pace presents (`specs/26` § 4 output row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PresentPolicy {
    /// Wait for vertical blank (FIFO). Pairs with a frame cap.
    #[default]
    Vsync,
    /// Uncapped. May tear (`Immediate`, else `AutoNoVsync`).
    Uncapped,
    /// Variable-refresh: mailbox when the surface has it, else adaptive FIFO, else FIFO.
    /// Does not tear when mailbox is available, except on DX12: there `Immediate` is chosen
    /// whenever the surface lists it, including when mailbox is also listed (tearing allowed).
    /// See `book/agents/swapchain.md`.
    Vrr,
}

/// Requested output dynamic range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HdrOutput {
    /// SDR sRGB.
    #[default]
    Sdr,
    /// HDR10 (BT.2100 PQ, `Rgb10a2Unorm`) when the surface lists it.
    Hdr10,
    /// scRGB (extended linear sRGB, `Rgba16Float`) when the surface lists it.
    ScRgb,
}

/// Swapchain format and colour space actually configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceChoice {
    /// Texture format of the swapchain.
    pub format: wgpu::TextureFormat,
    /// Colour space paired with that format.
    pub color_space: wgpu::SurfaceColorSpace,
    /// True when the choice is an HDR space (not the SDR fallback).
    pub hdr: bool,
}

/// Window output preferences. The default matches the pre-M15a path: vsync, SDR, no cap.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct OutputSettings {
    /// Present-mode policy.
    pub present: PresentPolicy,
    /// HDR request. Falls back to SDR when the surface cannot do it.
    pub hdr: HdrOutput,
    /// Frame cap in frames per second. `None` leaves [`regia_app::FramePacing`] alone.
    pub frame_cap_fps: Option<u32>,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            present: PresentPolicy::Vsync,
            hdr: HdrOutput::Sdr,
            frame_cap_fps: None,
        }
    }
}

impl OutputSettings {
    /// Output settings implied by a render profile (Deck caps at 40 fps and vsync).
    #[must_use]
    pub fn from_profile(profile: &RenderProfile) -> Self {
        Self {
            present: if profile.frame_cap_fps.is_some() {
                PresentPolicy::Vsync
            } else {
                PresentPolicy::Uncapped
            },
            hdr: HdrOutput::Sdr,
            frame_cap_fps: profile.frame_cap_fps,
        }
    }
}

/// Copies a profile frame cap into pacing and the output resource once.
///
/// A profile with no cap does not clear a cap the user already set. An existing
/// cap is left alone, and [`PresentPolicy::Vrr`] is not replaced. The scheduled
/// frame-cap system uses this same policy.
pub fn apply_profile_frame_cap(
    profile: &RenderProfile,
    pacing: &mut regia_app::FramePacing,
    output: &mut OutputSettings,
) {
    let Some(cap) = profile.frame_cap_fps else {
        return;
    };
    if output.frame_cap_fps.is_none() {
        output.frame_cap_fps = Some(cap);
        if output.present != PresentPolicy::Vrr {
            output.present = PresentPolicy::Vsync;
        }
    }
    if pacing.target_fps.is_none() {
        pacing.target_fps = Some(cap as f32);
    }
}

/// Picks a wgpu present mode for `policy` from the modes the surface listed.
///
/// `Mailbox` is preferred for [`PresentPolicy::Vrr`] (latest image, no tear, the
/// compositor can track a variable refresh). `Immediate` is the uncapped path.
#[must_use]
pub fn select_present_mode(policy: PresentPolicy, supported: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    let has = |mode: wgpu::PresentMode| supported.contains(&mode);
    match policy {
        PresentPolicy::Vsync => {
            if has(wgpu::PresentMode::Fifo) {
                wgpu::PresentMode::Fifo
            } else {
                wgpu::PresentMode::AutoVsync
            }
        }
        PresentPolicy::Uncapped => {
            if has(wgpu::PresentMode::Immediate) {
                wgpu::PresentMode::Immediate
            } else {
                wgpu::PresentMode::AutoNoVsync
            }
        }
        PresentPolicy::Vrr => {
            if has(wgpu::PresentMode::Mailbox) {
                wgpu::PresentMode::Mailbox
            } else if has(wgpu::PresentMode::FifoRelaxed) {
                wgpu::PresentMode::FifoRelaxed
            } else if has(wgpu::PresentMode::Fifo) {
                wgpu::PresentMode::Fifo
            } else {
                wgpu::PresentMode::AutoVsync
            }
        }
    }
}

/// Horizontal field of view (radians) for a vertical FOV and aspect.
#[must_use]
pub fn horizontal_fov(fov_y: f32, aspect: f32) -> f32 {
    2.0 * (aspect.max(1.0e-4) * (fov_y * 0.5).tan()).atan()
}

/// Vertical FOV to feed the projection so wider-than-16:9 views gain horizontal FOV
/// and narrower views keep the 16:9 horizontal FOV (horizontal-plus).
///
/// At exactly 16:9 the authored vertical FOV is unchanged.
#[must_use]
pub fn horizontal_plus_fov_y(fov_y: f32, aspect: f32) -> f32 {
    let aspect = aspect.max(1.0e-4);
    if aspect + 1.0e-4 >= HORIZONTAL_PLUS_REFERENCE_ASPECT {
        fov_y
    } else {
        let fov_x = horizontal_fov(fov_y, HORIZONTAL_PLUS_REFERENCE_ASPECT);
        2.0 * ((fov_x * 0.5).tan() / aspect).atan()
    }
}

/// Selects a swapchain format and colour space.
///
/// `color_spaces(format)` is what the surface reports for that format
/// ([`wgpu::SurfaceCapabilities::color_spaces`], empty for a format it does not list). The HDR
/// pairs are looked up there, not in `formats`: wgpu's `formats` holds only the formats `Auto`
/// can configure, so a format listed with an HDR color space alone is missing from it (ADR-525).
/// HDR requests that the surface does not list fall back to the first sRGB format (or the first
/// format) in SDR.
#[must_use]
pub fn select_surface_format(
    formats: &[wgpu::TextureFormat],
    color_spaces: impl Fn(wgpu::TextureFormat) -> wgpu::SurfaceColorSpaces,
    request: HdrOutput,
) -> SurfaceChoice {
    let sdr_format = formats
        .iter()
        .copied()
        .find(wgpu::TextureFormat::is_srgb)
        .or_else(|| formats.first().copied())
        .unwrap_or(wgpu::TextureFormat::Rgba8UnormSrgb);
    let sdr = SurfaceChoice {
        format: sdr_format,
        color_space: wgpu::SurfaceColorSpace::Srgb,
        hdr: false,
    };
    let pair_ok = |format: wgpu::TextureFormat, space: wgpu::SurfaceColorSpace| {
        space
            .to_color_spaces()
            .is_some_and(|flag| color_spaces(format).contains(flag))
    };
    match request {
        HdrOutput::Sdr => sdr,
        HdrOutput::Hdr10
            if pair_ok(
                wgpu::TextureFormat::Rgb10a2Unorm,
                wgpu::SurfaceColorSpace::Bt2100Pq,
            ) =>
        {
            SurfaceChoice {
                format: wgpu::TextureFormat::Rgb10a2Unorm,
                color_space: wgpu::SurfaceColorSpace::Bt2100Pq,
                hdr: true,
            }
        }
        HdrOutput::ScRgb
            if pair_ok(
                wgpu::TextureFormat::Rgba16Float,
                wgpu::SurfaceColorSpace::ExtendedSrgbLinear,
            ) =>
        {
            SurfaceChoice {
                format: wgpu::TextureFormat::Rgba16Float,
                color_space: wgpu::SurfaceColorSpace::ExtendedSrgbLinear,
                hdr: true,
            }
        }
        HdrOutput::Hdr10 | HdrOutput::ScRgb => sdr,
    }
}

/// SMPTE ST 2084 perceptual quantizer (HDR10). `nits` is absolute luminance.
///
/// `0` maps to 0 and `10_000` maps to 1. The signal is what an HDR10 swapchain
/// (`Bt2100Pq`) expects in `0.0..=1.0`.
#[must_use]
pub fn encode_pq_st2084(nits: f32) -> f32 {
    const M1: f32 = 0.159_301_76;
    const M2: f32 = 78.84375;
    const C1: f32 = 0.835_937_5;
    const C2: f32 = 18.851_562;
    const C3: f32 = 18.6875;
    let y = (nits / 10_000.0).clamp(0.0, 1.0);
    let y_m1 = y.powf(M1);
    ((C1 + C2 * y_m1) / (1.0 + C3 * y_m1)).powf(M2)
}

/// scRGB linear signal for an absolute luminance. IEC 61966-2-2: `1.0` is 80 cd/m².
#[must_use]
pub fn encode_scrgb(nits: f32) -> f32 {
    nits / 80.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_modes_match_the_policy() {
        let modes = [
            wgpu::PresentMode::Fifo,
            wgpu::PresentMode::FifoRelaxed,
            wgpu::PresentMode::Immediate,
            wgpu::PresentMode::Mailbox,
        ];
        assert_eq!(
            select_present_mode(PresentPolicy::Vsync, &modes),
            wgpu::PresentMode::Fifo
        );
        assert_eq!(
            select_present_mode(PresentPolicy::Uncapped, &modes),
            wgpu::PresentMode::Immediate
        );
        assert_eq!(
            select_present_mode(PresentPolicy::Vrr, &modes),
            wgpu::PresentMode::Mailbox
        );
        // No mailbox: VRR falls back to adaptive vsync, then FIFO.
        assert_eq!(
            select_present_mode(
                PresentPolicy::Vrr,
                &[wgpu::PresentMode::Fifo, wgpu::PresentMode::FifoRelaxed]
            ),
            wgpu::PresentMode::FifoRelaxed
        );
        assert_eq!(
            select_present_mode(PresentPolicy::Vrr, &[wgpu::PresentMode::Fifo]),
            wgpu::PresentMode::Fifo
        );
        assert_eq!(
            select_present_mode(PresentPolicy::Uncapped, &[wgpu::PresentMode::Fifo]),
            wgpu::PresentMode::AutoNoVsync
        );
    }

    #[test]
    fn ultrawide_keeps_vertical_fov_and_grows_horizontal() {
        let fov_y = 60.0_f32.to_radians();
        let wide = horizontal_plus_fov_y(fov_y, 32.0 / 9.0);
        assert!((wide - fov_y).abs() < 1.0e-5, "32:9 vertical FOV stays authored");
        let h16 = horizontal_fov(fov_y, 16.0 / 9.0);
        let h32 = horizontal_fov(wide, 32.0 / 9.0);
        assert!(h32 > h16 + 0.2, "32:9 horizontal FOV grows, {h16} -> {h32}");
        // 4:3 keeps the 16:9 horizontal FOV and opens the vertical.
        let narrow = horizontal_plus_fov_y(fov_y, 4.0 / 3.0);
        assert!(narrow > fov_y, "4:3 vertical FOV increases");
        let h4 = horizontal_fov(narrow, 4.0 / 3.0);
        assert!(
            (h4 - h16).abs() < 1.0e-4,
            "4:3 horizontal matches 16:9, {h4} vs {h16}"
        );
        let same = horizontal_plus_fov_y(fov_y, 16.0 / 9.0);
        assert!((same - fov_y).abs() < 1.0e-5);
    }

    #[test]
    fn hdr_selection_falls_back_when_the_surface_lacks_it() {
        let formats = [
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Bgra8Unorm,
        ];
        let none = |_format| wgpu::SurfaceColorSpaces::SRGB;
        let hdr10 = select_surface_format(&formats, none, HdrOutput::Hdr10);
        assert!(!hdr10.hdr);
        assert!(hdr10.format.is_srgb());

        let with_pq = |format| {
            if format == wgpu::TextureFormat::Rgb10a2Unorm {
                wgpu::SurfaceColorSpaces::BT2100_PQ
            } else {
                wgpu::SurfaceColorSpaces::SRGB
            }
        };
        let formats = [
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Rgb10a2Unorm,
            wgpu::TextureFormat::Rgba16Float,
        ];
        let pq = select_surface_format(&formats, with_pq, HdrOutput::Hdr10);
        assert!(pq.hdr);
        assert_eq!(pq.format, wgpu::TextureFormat::Rgb10a2Unorm);
        assert_eq!(pq.color_space, wgpu::SurfaceColorSpace::Bt2100Pq);

        let with_sc = |format| {
            if format == wgpu::TextureFormat::Rgba16Float {
                wgpu::SurfaceColorSpaces::EXTENDED_SRGB_LINEAR
            } else {
                wgpu::SurfaceColorSpaces::SRGB
            }
        };
        let sc = select_surface_format(&formats, with_sc, HdrOutput::ScRgb);
        assert!(sc.hdr);
        assert_eq!(sc.color_space, wgpu::SurfaceColorSpace::ExtendedSrgbLinear);
        // Requesting scRGB without the space listed stays SDR.
        let miss = select_surface_format(&formats, with_pq, HdrOutput::ScRgb);
        assert!(!miss.hdr);
    }

    #[test]
    fn pq_and_scrgb_endpoints() {
        assert!(encode_pq_st2084(0.0) < 1.0e-5);
        assert!((encode_pq_st2084(10_000.0) - 1.0).abs() < 1.0e-4);
        let nits100 = encode_pq_st2084(100.0);
        // Published ST 2084: 100 cd/m² encodes near 0.508.
        assert!(
            (0.50..0.52).contains(&nits100),
            "100 nits PQ {nits100} outside 0.50..0.52"
        );
        assert!(encode_pq_st2084(100.0) < encode_pq_st2084(1_000.0));
        assert!((encode_scrgb(80.0) - 1.0).abs() < 1.0e-5);
        assert!((encode_scrgb(0.0)).abs() < 1.0e-6);
        assert!(encode_scrgb(800.0) > 1.0, "highlights sit above SDR white");
    }

    #[test]
    fn frame_cap_policy_does_not_clear_an_existing_cap() {
        let mut pacing = regia_app::FramePacing::new(Some(120.0));
        let mut output = OutputSettings {
            present: PresentPolicy::Vrr,
            frame_cap_fps: Some(120),
            hdr: HdrOutput::Sdr,
        };
        let uncapped = RenderProfile::from_preset(crate::profile::RenderProfilePreset::DesktopHigh);
        apply_profile_frame_cap(&uncapped, &mut pacing, &mut output);
        assert_eq!(
            pacing.target_fps,
            Some(120.0),
            "a profile with no cap leaves pacing alone"
        );
        assert_eq!(output.frame_cap_fps, Some(120));
        assert_eq!(output.present, PresentPolicy::Vrr);
        let deck = RenderProfile::from_preset(crate::profile::RenderProfilePreset::Deck);
        apply_profile_frame_cap(&deck, &mut pacing, &mut output);
        assert_eq!(pacing.target_fps, Some(120.0), "an existing cap stays");
        assert_eq!(output.frame_cap_fps, Some(120));
        assert_eq!(output.present, PresentPolicy::Vrr, "VRR is not replaced");

        let mut fresh_pacing = regia_app::FramePacing::default();
        let mut fresh = OutputSettings::default();
        apply_profile_frame_cap(&deck, &mut fresh_pacing, &mut fresh);
        assert_eq!(fresh_pacing.target_fps, Some(40.0));
        assert_eq!(fresh.frame_cap_fps, Some(40));
        assert_eq!(fresh.present, PresentPolicy::Vsync);
    }
}
