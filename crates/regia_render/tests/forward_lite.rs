// SPDX-License-Identifier: MPL-2.0
//! The lite forward path (ADR-1874): an adapter outside the tile GPUs keeps the full shader at the
//! Mobile rows, byte for byte, and each forced lite tier draws the same scene closely (no golden:
//! it is compared with the full shader's frame of the same scene).

use regia_render::forward_lite::ForwardLite;
use regia_render::hard_surface::{HardSurfaceFeature, feature_demo_scene};
use regia_render::{
    ALPHA_MASK, FrameQuality, FrameRenderer, LabLocalLight, LabMaterial, LabPointLight, LabScene, LabTexture,
    RenderDevice, RenderProfile, RenderProfilePreset,
};

const MOBILE: [RenderProfilePreset; 2] = [RenderProfilePreset::MobileLow, RenderProfilePreset::MobileHigh];

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip forward_lite: {err}");
            None
        }
    }
}

fn quality(preset: RenderProfilePreset) -> FrameQuality {
    FrameQuality::from_profile(&RenderProfile::from_preset(preset))
}

/// A 32x32 mask: opaque stripes with transparent gaps.
fn stripes() -> LabTexture {
    let mut rgba = Vec::with_capacity(32 * 32 * 4);
    for _y in 0..32u32 {
        for x in 0..32u32 {
            let a = if (x / 4).is_multiple_of(2) { 255 } else { 0 };
            rgba.extend_from_slice(&[200, 180, 140, a]);
        }
    }
    LabTexture {
        width: 32,
        height: 32,
        rgba,
    }
}

/// The plain hard-surface scene with a masked copy of its last mesh.
fn scene() -> LabScene {
    let mut scene = feature_demo_scene(HardSurfaceFeature::Detail, true);
    scene.width = 320;
    scene.height = 180;
    scene.materials.push(LabMaterial {
        alpha_mode: ALPHA_MASK,
        alpha_cutoff: 0.5,
        albedo: Some(stripes()),
        ..LabMaterial::default()
    });
    let mask_index = scene.materials.len() - 1;
    if let Some(mut copy) = scene.meshes.last().cloned() {
        copy.material = mask_index;
        copy.model[3][0] += 0.35;
        copy.model[3][2] += 0.3;
        scene.meshes.push(copy);
    }
    scene
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
        / rgba.len() as f64
}

/// The frame's mean red minus its mean blue (0-255).
fn red_minus_blue(pixels: &[u8]) -> f64 {
    let (rgba, _) = pixels.as_chunks::<4>();
    rgba.iter()
        .map(|p| f64::from(p[0]) - f64::from(p[2]))
        .sum::<f64>()
        / rgba.len() as f64
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
    close as f64 / a.len() as f64
}

#[test]
fn a_desktop_adapter_keeps_the_full_shader_at_the_mobile_rows() {
    let Some(device) = device() else { return };
    let scene = scene();
    for preset in MOBILE {
        let auto = render(&device, &scene, quality(preset), None);
        let off = render(&device, &scene, quality(preset), Some(ForwardLite::Off));
        assert_eq!(auto, off, "{preset:?} changed pixels on a desktop adapter");
    }
}

#[test]
fn the_lite_tiers_draw_the_scene_close_to_the_full_shader() {
    let Some(device) = device() else { return };
    let scene = scene();
    for (preset, tier) in [
        (RenderProfilePreset::MobileLow, ForwardLite::Low),
        (RenderProfilePreset::MobileHigh, ForwardLite::High),
    ] {
        let full = render(&device, &scene, quality(preset), Some(ForwardLite::Off));
        let lite = render(&device, &scene, quality(preset), Some(tier));
        assert_ne!(full, lite, "{tier:?} drew the full shader");
        let (a, b) = (luma(&full), luma(&lite));
        assert!(
            (a - b).abs() < 0.2 * a.max(1.0),
            "{tier:?} mean luma {b:.1} vs full {a:.1}"
        );
        // Covers the mask's holes and the sun shadow: either one lost shows as whole regions.
        let close = close_share(&full, &lite, 40);
        assert!(
            close > 0.8,
            "{tier:?}: only {:.1} % of pixels close",
            close * 100.0
        );
    }
}

#[test]
fn a_local_light_reaches_the_lite_tiers() {
    let Some(device) = device() else { return };
    let dark = scene();
    let mut lit = scene();
    let target = lit.target;
    lit.local_lights.push(LabLocalLight::Point(LabPointLight {
        position: [target[0] - 1.2, target[1] + 1.0, target[2] + 1.0],
        color: [1.0, 0.2, 0.1],
        intensity: 200_000.0,
        radius: 8.0,
        cast_shadows: false,
        source_radius: None,
    }));
    for (preset, tier) in [
        (RenderProfilePreset::MobileLow, ForwardLite::Low),
        (RenderProfilePreset::MobileHigh, ForwardLite::High),
    ] {
        // How far the red light moves the frame's mean red-minus-blue, full shader and lite tier.
        let warmed = |shader: ForwardLite| {
            let without = render(&device, &dark, quality(preset), Some(shader));
            let with = render(&device, &lit, quality(preset), Some(shader));
            red_minus_blue(&with) - red_minus_blue(&without)
        };
        let (full, lite) = (warmed(ForwardLite::Off), warmed(tier));
        assert!(
            full > 0.1,
            "the fixture's light warmed the full shader's frame by only {full:.3}"
        );
        assert!(
            lite > 0.5 * full && lite < 2.0 * full,
            "{tier:?}: the light warmed the frame by {lite:.3}, the full shader's by {full:.3}"
        );
    }
}

#[test]
fn the_lite_compositions_stay_inside_the_declared_axes() {
    let Some(device) = device() else { return };
    let scene = scene();
    render(
        &device,
        &scene,
        quality(RenderProfilePreset::MobileLow),
        Some(ForwardLite::Low),
    );
    render(
        &device,
        &scene,
        quality(RenderProfilePreset::MobileHigh),
        Some(ForwardLite::High),
    );
    let report = regia_render::forward_permutation_report();
    assert!(
        report.undeclared.is_empty(),
        "undeclared: {:?}",
        report.undeclared
    );
    let lite = report
        .permutations
        .iter()
        .filter(|set| set.iter().any(|d| d.starts_with("FORWARD_LITE_")))
        .count();
    assert!(lite >= 2, "two tiers drew, {lite} lite sets recorded");
    assert!(lite <= 8, "{lite} lite sets: tier x alpha test x encode is 8");
}
