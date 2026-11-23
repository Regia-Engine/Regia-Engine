// SPDX-License-Identifier: MPL-2.0
//! A lite tier forced onto a frame that plans more sun cascades than the tier's profile (ADR-2403):
//! the lite shader reads every cascade the frame planned, so the sun's shadows past cascade 0 stay.
//!
//! The Mobile profiles plan the cascades their tier reads (Mobile Low 1, Mobile High 2). A host
//! that forces a tier onto another profile (`FrameRenderer::set_forward_lite`, `REGIA_FORWARD_LITE`,
//! the lightmap evidence of `docs/evidence/lightmaps-v030`) used to get the sun unshadowed past
//! cascade 0 (Low) or 1 (High): the colour room's shaded back wall and floor drew in full sun, which
//! the evidence measured as 2.3 times the indirect light on the lite `LIGHTMAP` receivers.
//!
//! The scene is the colour room of the GI set (`regia_test_utils::gi_scenes::color_room`) at Desktop
//! High (4 cascades), on the CPU colour loop (the only path the lite shader draws). No golden: each
//! lite frame is compared with the full shader's frame of the same scene (lavapipe, deterministic).

use regia_render::forward_lite::ForwardLite;
use regia_render::{
    FrameQuality, FrameRenderer, GiMode, LabScene, RenderDevice, RenderProfile, RenderProfilePreset,
};
use regia_test_utils::gi_scenes::color_room;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// `preset` on the CPU colour loop at native scale, with the flat ambient as its GI (the lightmap
/// evidence's frame).
fn quality(preset: RenderProfilePreset) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(preset));
    q.gpu_driven = false;
    q.visibility_buffer = false;
    q.taau = false;
    q.msaa_samples = 1;
    q.render_scale_permille = 1000;
    q.horizontal_plus = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.ssr = false;
    q.reflection_probes = false;
    q.gi = GiMode::Ambient;
    q.screen_space_gi = false;
    q.gi_probes = false;
    q.baked_gi = false;
    q
}

fn render(
    device: &RenderDevice,
    scene: &LabScene,
    quality: FrameQuality,
    lite: Option<ForwardLite>,
) -> Vec<u8> {
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(lite);
    let frame = scene.as_frame_scene_with_quality(quality);
    renderer
        .render_offscreen(device, &frame)
        .expect("offscreen frame")
        .pixels
}

fn luma(pixels: &[u8]) -> f64 {
    let (rgba, _) = pixels.as_chunks::<4>();
    rgba.iter()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .sum::<f64>()
        / rgba.len().max(1) as f64
}

/// Share of pixels with every channel within `tolerance`.
fn close_share(a: &[u8], b: &[u8], tolerance: u8) -> f64 {
    let (a, _) = a.as_chunks::<4>();
    let (b, _) = b.as_chunks::<4>();
    let close = a
        .iter()
        .zip(b)
        .filter(|(x, y)| (0..3).all(|c| x[c].abs_diff(y[c]) <= tolerance))
        .count();
    close as f64 / a.len().max(1) as f64
}

/// FNV-1a over the frame: printed so a run on another tree can compare the Mobile frames byte for
/// byte (they must not move with ADR-2403).
fn fnv(pixels: &[u8]) -> u64 {
    pixels.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// How far a forced lite tier's mean luma may sit from the full shader's on the same cascades, as
/// a share of it. Measured on lavapipe (the build server, 2026-11-03): Low 147.5 and High 147.6 against
/// 146.8 (0.5 %); before ADR-2403 Low drew 167.7 (+14 %), the sun unshadowed past cascade 0.
const LUMA_SHARE: f64 = 0.03;
/// Pixels whose every channel is within `CLOSE_TOLERANCE` of the full shader's: 99.6 % at both
/// tiers (the lite sun's single tap moves shadow edges by a pixel); 79.1 % before ADR-2403.
const CLOSE_SHARE: f64 = 0.95;
const CLOSE_TOLERANCE: u8 = 24;
/// `forward_lite.rs`'s bound for a lite tier on its own Mobile row.
const MOBILE_LUMA_SHARE: f64 = 0.2;

#[test]
fn a_forced_lite_tier_keeps_the_sun_shadows_of_every_planned_cascade() {
    let device = device();
    let scene = color_room().lab;
    let desktop = quality(RenderProfilePreset::DesktopHigh);
    assert!(
        desktop.csm_cascade_count > 2,
        "the fixture plans more cascades than either lite tier's profile: {}",
        desktop.csm_cascade_count
    );
    let full = render(&device, &scene, desktop, Some(ForwardLite::Off));
    for tier in [ForwardLite::Low, ForwardLite::High] {
        let lite = render(&device, &scene, desktop, Some(tier));
        assert_ne!(full, lite, "{tier:?} drew the full shader");
        let (a, b) = (luma(&full), luma(&lite));
        let close = close_share(&full, &lite, CLOSE_TOLERANCE);
        eprintln!(
            "{tier:?} on {} cascades: mean luma {b:.1}, full {a:.1}; {:.1} % of pixels close",
            desktop.csm_cascade_count,
            close * 100.0
        );
        assert!(
            (b - a).abs() <= LUMA_SHARE * a,
            "{tier:?} on {} cascades: mean luma {b:.1} against the full shader's {a:.1} (the sun past \
             the tier's own cascades lost its shadow?)",
            desktop.csm_cascade_count
        );
        assert!(
            close >= CLOSE_SHARE,
            "{tier:?}: only {:.1} % of pixels within {CLOSE_TOLERANCE} of the full shader's",
            close * 100.0
        );
    }
}

/// The lite tiers on their own Mobile rows against the full shader, as `forward_lite.rs` bounds
/// them (a fifth of the mean luma). These frames plan the cascades their tier reads, so ADR-2403
/// does not move them; the hashes are printed for the byte-for-byte check against another tree
/// (lavapipe on the build server, 2026-11-03, before and after: Mobile Low's lite frame `9e3b5bd0f4db1957`,
/// Mobile High's `cfbc42c5cf7dc5bd`). The room's Mobile Low lite frame is 10 % darker than the full
/// shader's (106.5 against 118.8) with or without ADR-2403; Mobile High's is within 0.1 %.
#[test]
fn the_mobile_rows_keep_their_lite_frames() {
    let device = device();
    let scene = color_room().lab;
    let rows: Vec<(ForwardLite, f64, f64)> = [
        (RenderProfilePreset::MobileLow, ForwardLite::Low),
        (RenderProfilePreset::MobileHigh, ForwardLite::High),
    ]
    .into_iter()
    .map(|(preset, tier)| {
        let q = quality(preset);
        let full = render(&device, &scene, q, Some(ForwardLite::Off));
        let lite = render(&device, &scene, q, Some(tier));
        eprintln!(
            "{preset:?} ({} cascades): full {:016x}, {tier:?} {:016x}; mean luma {:.1}, full {:.1}",
            q.csm_cascade_count,
            fnv(&full),
            fnv(&lite),
            luma(&lite),
            luma(&full)
        );
        (tier, luma(&full), luma(&lite))
    })
    .collect();
    for (tier, a, b) in rows {
        assert!(
            (b - a).abs() <= MOBILE_LUMA_SHARE * a,
            "{tier:?}: mean luma {b:.1} against {a:.1}"
        );
    }
}
