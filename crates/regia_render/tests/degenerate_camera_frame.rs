// SPDX-License-Identifier: MPL-2.0
//! Degenerate cameras render a usable frame (ADR-869), on lavapipe in CI.
//!
//! - `straight_down_camera_matches_a_tiny_tilt`: a camera looking exactly along −Y (glam's
//!   look-at is NaN there: forward × up is zero) draws the same picture as one tilted by
//!   1e-3 rad toward −Z, whose look-at is ordinary.
//! - `infinite_far_plane_draws_the_scene`: `far: f32::INFINITY` (the `Camera::far` docs
//!   allow it; the perspective matrix was NaN) draws the same picture as the 100 km stand-in.
//! - `broken_cameras_render_and_leave_no_state_behind`: a NaN eye, an eye on its target, a
//!   zero near plane and a zero field of view each render without an error, and the next
//!   ordinary frame on that renderer equals the same frame on a fresh one.
//! - `desktop_high_hdr_stays_finite_for_degenerate_views`: at Desktop High, a zenith sun, a
//!   straight-down camera and an infinite far plane leave every linear HDR texel finite.
//!
//! No goldens: every check compares two renders of this test.

use regia_render::view_math::INFINITE_FAR_STANDIN;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameScene, LabMaterial, LabMesh, LabScene,
    LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 128;
const H: u32 = 96;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A horizontal quad of half size `half` centered at `(x, y, z)`, facing +Y.
fn slab(x: f32, y: f32, z: f32, half: f32, material: usize) -> LabMesh {
    let corners = [
        [x - half, y, z - half],
        [x - half, y, z + half],
        [x + half, y, z + half],
        [x + half, y, z - half],
    ];
    LabMesh {
        vertices: corners
            .iter()
            .map(|&p| LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], [0.0, 0.0]))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

/// A grey ground with an orange block east of the center and a blue one north of it, so a
/// mirrored or rotated picture would not match.
fn scene(eye: [f32; 3], target: [f32; 3]) -> LabScene {
    let material = |rgb: [f32; 3]| LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        roughness: 0.7,
        ..LabMaterial::default()
    };
    LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 4_000.0,
        double_sided: true,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([-0.3, -1.0, -0.2], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![
            material([0.5, 0.5, 0.5]),
            material([0.9, 0.45, 0.1]),
            material([0.1, 0.3, 0.9]),
        ],
        meshes: vec![
            slab(0.0, 0.0, 0.0, 20.0, 0),
            slab(2.0, 0.5, 0.0, 1.0, 1),
            slab(0.0, 0.5, -2.5, 0.7, 2),
        ],
    }
}

fn render(renderer: &mut FrameRenderer, device: &RenderDevice, frame: &FrameScene<'_>) -> Vec<u8> {
    renderer.render_offscreen(device, frame).expect("frame").pixels
}

fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sum: u64 = a.iter().zip(b).map(|(x, y)| u64::from(x.abs_diff(*y))).sum();
    sum as f64 / a.len() as f64
}

/// Mean Rec. 709 luma of the picture, 0..255.
fn mean_luma(px: &[u8]) -> f64 {
    let (pixels, _) = px.as_chunks::<4>();
    let sum: f64 = pixels
        .iter()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .sum();
    sum / pixels.len().max(1) as f64
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn straight_down_camera_matches_a_tiny_tilt() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let eye = [0.0, 10.0, 0.0];
    let exact = scene(eye, [0.0, 0.0, 0.0]);
    // 1e-3 rad toward −Z: 1 cm at the ground, about a tenth of a pixel here.
    let tilted = scene(eye, [0.0, 0.0, -0.01]);
    let exact_px = render(
        &mut FrameRenderer::new(),
        &device,
        &exact.as_frame_scene_with_quality(FrameQuality::LAB),
    );
    let tilted_px = render(
        &mut FrameRenderer::new(),
        &device,
        &tilted.as_frame_scene_with_quality(FrameQuality::LAB),
    );
    let luma = mean_luma(&exact_px);
    let diff = mean_abs_diff(&exact_px, &tilted_px);
    eprintln!("straight down: luma {luma:.1}, mean |Δ| vs tilt {diff:.3}");
    assert!(luma > 20.0, "the ground must be drawn and lit: luma {luma:.1}");
    assert!(
        diff < 1.5,
        "straight down must match a 1e-3 rad tilt: mean |Δ| {diff:.3}"
    );
}

#[test]
fn infinite_far_plane_draws_the_scene() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene([0.0, 4.0, 6.0], [0.0, 0.0, 0.0]);
    let mut infinite = lab.as_frame_scene_with_quality(FrameQuality::LAB);
    infinite.far = f32::INFINITY;
    let mut standin = lab.as_frame_scene_with_quality(FrameQuality::LAB);
    standin.far = INFINITE_FAR_STANDIN;
    let infinite_px = render(&mut FrameRenderer::new(), &device, &infinite);
    let standin_px = render(&mut FrameRenderer::new(), &device, &standin);
    let luma = mean_luma(&infinite_px);
    let diff = mean_abs_diff(&infinite_px, &standin_px);
    eprintln!("infinite far: luma {luma:.1}, mean |Δ| vs the stand-in {diff:.3}");
    assert!(luma > 20.0, "the scene must be drawn: luma {luma:.1}");
    assert!(
        diff < 0.5,
        "far = ∞ must render as the stand-in far plane: mean |Δ| {diff:.3}"
    );
}

#[test]
fn broken_cameras_render_and_leave_no_state_behind() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene([0.0, 4.0, 6.0], [0.0, 0.0, 0.0]);
    let good = lab.as_frame_scene_with_quality(FrameQuality::LAB);
    let mut renderer = FrameRenderer::new();
    let broken: [fn(&mut FrameScene<'_>); 6] = [
        |f| f.eye = [f32::NAN, 1.0, 2.0],
        |f| f.target = f.eye,
        |f| f.target = [f32::INFINITY, 0.0, 0.0],
        |f| f.near = 0.0,
        |f| f.fov_y = 0.0,
        |f| {
            f.near = f32::NAN;
            f.far = f32::NEG_INFINITY;
            f.fov_y = f32::NAN;
        },
    ];
    for (k, break_it) in broken.iter().enumerate() {
        let mut frame = good.clone();
        break_it(&mut frame);
        if let Err(err) = renderer.render_offscreen(&device, &frame) {
            panic!("broken camera {k} must still render: {err}");
        }
    }
    let after = render(&mut renderer, &device, &good);
    let fresh = render(&mut FrameRenderer::new(), &device, &good);
    let diff = mean_abs_diff(&after, &fresh);
    eprintln!("after broken cameras: mean |Δ| vs a fresh renderer {diff:.3}");
    assert!(mean_luma(&after) > 20.0);
    assert!(
        diff < 0.5,
        "a broken camera left state behind: mean |Δ| {diff:.3}"
    );
}

/// The linear HDR scene color of the third frame (so temporal passes have a history) at
/// Desktop High quality: every pass the preset runs, before exposure and the tonemap.
fn desktop_high_hdr(device: &RenderDevice, frame: &FrameScene<'_>) -> Vec<[f32; 3]> {
    let mut renderer = FrameRenderer::new();
    for _ in 0..2 {
        render(&mut renderer, device, frame);
    }
    renderer.request_hdr_capture();
    render(&mut renderer, device, frame);
    match renderer.take_hdr_capture(device) {
        Ok(Some(capture)) => capture.rgb,
        Ok(None) => panic!("no HDR capture (the post stack did not run)"),
        Err(err) => panic!("HDR capture: {err}"),
    }
}

/// End to end at Desktop High (TAA, sun cascades, probe GI, the LUT sky): a sun exactly at the
/// zenith (its cascade look-at was NaN, and the cascades' own fallback direction is straight
/// down), a camera looking straight down, and an infinite far plane each leave every HDR texel
/// finite and the picture lit.
#[test]
fn desktop_high_hdr_stays_finite_for_degenerate_views() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let mut zenith = scene([0.0, 4.0, 6.0], [0.0, 0.0, 0.0]);
    zenith.directional_shadows = true;
    zenith.directional = DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 20_000.0);
    let mut down = scene([0.0, 10.0, 0.0], [0.0, 0.0, 0.0]);
    down.directional_shadows = true;
    let mut infinite = scene([0.0, 4.0, 6.0], [0.0, 0.0, 0.0]);
    infinite.directional_shadows = true;
    infinite.far = f32::INFINITY;
    for (name, lab) in [
        ("zenith sun", zenith),
        ("straight down", down),
        ("infinite far", infinite),
    ] {
        let frame = lab.as_frame_scene_with_quality(quality);
        let hdr = desktop_high_hdr(&device, &frame);
        let bad = hdr.iter().filter(|c| c.iter().any(|v| !v.is_finite())).count();
        let mean = hdr.iter().map(|c| f64::from(c[0] + c[1] + c[2])).sum::<f64>() / hdr.len().max(1) as f64;
        eprintln!(
            "{name}: {} HDR texels, {bad} non-finite, mean {mean:.3}",
            hdr.len()
        );
        assert_eq!(bad, 0, "{name}: non-finite HDR texels");
        assert!(mean > 0.0, "{name}: the HDR frame is black");
    }
}
