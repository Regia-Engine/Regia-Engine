// SPDX-License-Identifier: MPL-2.0
//! FSR 3.1 is opt-in (ADR-113, ADR-041): frames that do not select it are byte-identical,
//! and a frame that selects it really runs the FSR 3.1 passes.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, Fsr3Settings, LabMaterial, LabMesh, LabScene,
    LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, TemporalUpscaler,
};

const W: u32 = 160;
const H: u32 = 90;

fn quad(corners: [[f32; 3]; 4], normal: [f32; 3], material: usize) -> LabMesh {
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material,
        ..LabMesh::unskinned()
    }
}

fn scene(width: u32, height: u32) -> LabScene {
    // A slab standing 6 m in front of the eye, rotated so its edges are slanted on screen
    // (about 1 in 7 for the top and bottom, steep for the sides).
    let (s, c) = (0.14f32.sin(), 0.14f32.cos());
    let rot = |x: f32, y: f32| [x * c - y * s, 1.5 + x * s + y * c, -6.0];
    LabScene {
        width,
        height,
        eye: [0.0, 1.5, 0.0],
        target: [0.0, 1.5, -6.0],
        fov_y: 0.9,
        near: 0.1,
        far: 500.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.5, -0.8], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.03, 0.03, 0.03, 1.0],
                roughness: 1.0,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.8, 0.8, 1.0],
                roughness: 1.0,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![
            // Dark backdrop.
            quad(
                [
                    [-40.0, -20.0, -12.0],
                    [40.0, -20.0, -12.0],
                    [40.0, 25.0, -12.0],
                    [-40.0, 25.0, -12.0],
                ],
                [0.0, 0.0, 1.0],
                0,
            ),
            // The bright slab.
            quad(
                [rot(-2.2, -1.0), rot(2.2, -1.0), rot(2.2, 1.0), rot(-2.2, 1.0)],
                [0.0, 0.0, 1.0],
                1,
            ),
        ],
    }
}

fn post() -> PostSettings {
    PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        taa: false,
        ..PostSettings::golden_deterministic()
    }
}

fn quality(taa: bool, scale: f32) -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.taau = taa;
    q.render_scale_permille = (scale * 1000.0).round() as u16;
    q
}

/// Renders `frames` frames of the slab scene and returns the last frame's pixels.
fn render(device: &RenderDevice, renderer: &mut FrameRenderer, q: FrameQuality, frames: usize) -> Vec<u8> {
    let post = post();
    let s = scene(W, H);
    let mut frame = s.as_frame_scene_with_quality(q);
    frame.post = Some(&post);
    let mut last = Vec::new();
    for _ in 0..frames {
        last = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    last
}

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    }
}

fn tuned() -> Fsr3Settings {
    Fsr3Settings {
        sharpness: 0.1,
        debug_view: 3,
        ..Fsr3Settings::default()
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn fsr3_settings_leave_the_default_upscaler_byte_identical() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let q = quality(true, 0.67);
    let mut plain = FrameRenderer::new();
    let mut tuned_renderer = FrameRenderer::new();
    tuned_renderer.set_fsr3_settings(tuned());
    assert_eq!(tuned_renderer.temporal_upscaler(), TemporalUpscaler::Taau);
    let a = render(&device, &mut plain, q, 8);
    let b = render(&device, &mut tuned_renderer, q, 8);
    assert!(a == b, "FSR 3.1 tuning changed a TAAU frame");
}

#[test]
fn selecting_fsr3_leaves_frames_without_taa_byte_identical() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let q = quality(false, 1.0);
    let mut plain = FrameRenderer::new();
    let mut fsr3 = FrameRenderer::new();
    fsr3.set_temporal_upscaler(TemporalUpscaler::Fsr3);
    fsr3.set_fsr3_settings(tuned());
    let a = render(&device, &mut plain, q, 3);
    let b = render(&device, &mut fsr3, q, 3);
    assert!(a == b, "selecting FSR 3.1 changed a frame without TAA");
}

#[test]
fn fsr3_resolves_a_taa_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let q = quality(true, 0.67);
    let mut taau = FrameRenderer::new();
    let mut fsr3 = FrameRenderer::new();
    fsr3.set_temporal_upscaler(TemporalUpscaler::Fsr3);
    let a = render(&device, &mut taau, q, 16);
    let b = render(&device, &mut fsr3, q, 16);
    assert_eq!(a.len(), b.len());
    assert!(
        a != b,
        "the FSR 3.1 frame equals the TAAU frame: the passes did not run"
    );
    // The same scene: the mean luma agrees within a few levels (exposure, no NaN, no black).
    let mean =
        |p: &[u8]| p.as_chunks::<4>().0.iter().map(|c| f32::from(c[1])).sum::<f32>() / (p.len() / 4) as f32;
    let (ma, mb) = (mean(&a), mean(&b));
    assert!((ma - mb).abs() < 4.0, "TAAU mean {ma:.1} vs FSR 3.1 {mb:.1}");
    // The disocclusion debug view replaces the image: a converged held view is ~0 there.
    fsr3.set_fsr3_settings(Fsr3Settings {
        debug_view: 3,
        ..Fsr3Settings::default()
    });
    let d = render(&device, &mut fsr3, q, 2);
    assert!(mean(&d) < 0.5 * mb, "debug view mean {:.1}", mean(&d));
}
