// SPDX-License-Identifier: MPL-2.0
//! The LOD cross-fade dither under a moving temporal resolve leaves no 1 px stipple (ADR-3146,
//! lavapipe, no goldens).
//!
//! An opaque white card fills the view with a LOD fade code, as one of the two copies of an instance
//! in a cross-fade or the mesh side of the impostor hand-over. The camera trucks 0.37 px a frame for
//! 24 frames and the last resolved frame is measured with
//! `regia_render::alpha_hash::stipple_energy`: the same spectral test that proved the hashed alpha
//! fix (ADR-1489). With interleaved gradient noise as the threshold about half of the card's
//! fine-scale power sits on the IGN lattice; the hierarchy phase leaves noise without a peak. A held view
//! keeps IGN, so that a held sequence renders what it rendered before this change. In this harness a
//! fading card draws whole without temporal AA (on the base too, every frame, opaque or masked; reported
//! in ADR-3146), so the per-pixel checks of the threshold live in `regia_terrain`'s
//! `scatter_impostor_stipple`, whose scatter meshes do dither without it.

use regia_render::alpha_hash::stipple_energy;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 160;
const H: u32 = 120;
/// The measured square, centred in the frame.
const SIDE: usize = 96;
const FRAMES: u32 = 24;
const FOV_Y: f32 = 0.8;
/// Card distance from the eye, metres.
const DISTANCE: f32 = 10.0;
/// Half of the pixels: the incoming copy of an instance halfway through its fade.
const CODE: f32 = 0.5;

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A 40 m square card in the XY plane at z = 0 facing +Z.
fn card() -> LabMesh {
    let half = 20.0;
    let corners = [
        ([-half, -half, 0.0], [0.0, 1.0]),
        ([half, -half, 0.0], [1.0, 1.0]),
        ([half, half, 0.0], [1.0, 0.0]),
        ([-half, half, 0.0], [0.0, 0.0]),
    ];
    LabMesh {
        vertices: corners
            .iter()
            .map(|&(position, uv)| LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

/// One image per entry of `eye_x`, the eye at that X (the card at one depth, so the whole card moves
/// alike), the card carrying the fade `code`.
fn render(device: &RenderDevice, code: f32, taa: bool, eye_x: &[f32]) -> Vec<Vec<u8>> {
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 12.0,
        bloom: false,
        taa,
        ..PostSettings::golden_deterministic()
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.render_scale_permille = 1000;
    quality.gpu_driven = false;
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.gi_probes = false;
    quality.taau = taa;
    quality.smaa = false;
    let mut renderer = FrameRenderer::new();
    let mut images = Vec::with_capacity(eye_x.len());
    for (frame, &x) in eye_x.iter().enumerate() {
        let lab = LabScene {
            width: W,
            height: H,
            eye: [x, 0.0, DISTANCE],
            target: [x, 0.0, 0.0],
            fov_y: FOV_Y,
            near: 0.1,
            far: 100.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 0.0,
            double_sided: true,
            post: None,
            directional: DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0, 1.0, 1.0], 20_000.0),
            local_lights: Vec::new(),
            materials: vec![LabMaterial {
                base_color: [1.0, 1.0, 1.0, 1.0],
                roughness: 1.0,
                double_sided: true,
                ..LabMaterial::default()
            }],
            meshes: vec![card()],
        };
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&post);
        for mesh in &mut scene.meshes {
            mesh.lod_dither = code;
        }
        let image = renderer
            .render_offscreen(device, &scene)
            .unwrap_or_else(|err| panic!("frame {frame}: {err}"));
        images.push(image.pixels);
    }
    images
}

/// Eye positions of a truck at `px_per_frame`.
fn truck(px_per_frame: f32) -> Vec<f32> {
    let metres_per_px = 2.0 * DISTANCE * (FOV_Y * 0.5).tan() / H as f32;
    (0..FRAMES)
        .map(|frame| frame as f32 * px_per_frame * metres_per_px)
        .collect()
}

/// Linear luminance of the measured square.
fn measured_luma(pixels: &[u8]) -> Vec<f32> {
    let (x0, y0) = ((W as usize - SIDE) / 2, (H as usize - SIDE) / 2);
    let lin = |c: u8| {
        let c = f32::from(c) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let mut luma = Vec::with_capacity(SIDE * SIDE);
    for y in y0..y0 + SIDE {
        for x in x0..x0 + SIDE {
            let i = (y * W as usize + x) * 4;
            luma.push(0.2126 * lin(pixels[i]) + 0.7152 * lin(pixels[i + 1]) + 0.0722 * lin(pixels[i + 2]));
        }
    }
    luma
}

#[test]
fn a_moving_lod_fade_resolves_without_a_lattice_stipple() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let frames = render(&device, CODE, true, &truck(0.37));
    let luma = measured_luma(frames.last().map_or(&[][..], Vec::as_slice));
    let energy = stipple_energy(&luma, SIDE, SIDE);
    let mean = luma.iter().sum::<f32>() / luma.len() as f32;
    eprintln!("LOD fade 0.5, 0.37 px/frame: {energy:?}; mean luma {mean:.4}");
    assert!(mean > 0.01, "the fading card must draw ({mean})");
    assert!(
        energy.lattice < 0.08,
        "the resolved fade keeps {:.1} % of its fine-scale power on the IGN lattice: {energy:?}",
        energy.lattice * 100.0
    );
    assert!(
        energy.checker < 0.08,
        "the resolved fade keeps a (-1)^(x+y) checkerboard: {energy:?}"
    );
    assert!(
        energy.peak_ratio < 80.0,
        "one frequency dominates the resolved fade (a lattice): {energy:?}"
    );
}

/// A held view keeps interleaved gradient noise: its converged residual stays on the IGN lattice, the
/// evidence that the held flag reached the dither.
#[test]
fn a_held_lod_fade_keeps_the_interleaved_gradient_phase() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let measure = |px: f32| {
        let frames = render(&device, CODE, true, &truck(px));
        stipple_energy(
            &measured_luma(frames.last().map_or(&[][..], Vec::as_slice)),
            SIDE,
            SIDE,
        )
    };
    let held = measure(0.0);
    let moving = measure(0.37);
    eprintln!("LOD fade held: {held:?}; moving: {moving:?}");
    assert!(
        held.peak_ratio > 150.0 && held.peak_ratio > 3.0 * moving.peak_ratio,
        "a held view should keep the IGN phase (its lattice peak): held {held:?}, moving {moving:?}"
    );
}
