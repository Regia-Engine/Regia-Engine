// SPDX-License-Identifier: MPL-2.0
//! The hashed alpha test under a moving temporal resolve leaves no 1 px stipple (ADR-1489,
//! lavapipe, no goldens).
//!
//! A white masked card with 1-texel alpha stripes fills the view: every mip from 1 down holds
//! alpha 0.5, so the whole card takes the hashed threshold (`alpha_rejects`, on with
//! `FrameRenderer::set_taa_hashed_alpha`). The camera trucks 0.37 px a frame for 24 frames, as a
//! showcase still's settle frames do, and the last resolved frame is measured with
//! `regia_render::alpha_hash::stipple_energy`. With interleaved gradient noise as the phase
//! (before ADR-1489) about half of the card's fine-scale power sat on the IGN lattice, the
//! `(-1)^(x+y)`-like stipple of the look-round-4 oak crown; the hierarchy phase leaves noise
//! without a peak.

use regia_render::alpha_hash::{StippleEnergy, stipple_energy};
use regia_render::{
    ALPHA_MASK, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabTexture, LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 160;
const H: u32 = 120;
/// The measured square, centred in the frame.
const SIDE: usize = 96;
const FRAMES: u32 = 24;
const FOV_Y: f32 = 0.8;
/// Card distance from the eye, metres.
const DISTANCE: f32 = 10.0;

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A 16 × 16 white texture whose alpha alternates 255 / `odd` column by column: with `odd` 0
/// every mip below the top averages to alpha 0.5.
fn stripes(odd: u8) -> LabTexture {
    let mut rgba = Vec::with_capacity(16 * 16 * 4);
    for _row in 0..16 {
        for col in 0..16 {
            rgba.extend_from_slice(&[255, 255, 255, if col % 2 == 0 { 255 } else { odd }]);
        }
    }
    LabTexture {
        width: 16,
        height: 16,
        rgba,
    }
}

/// A 40 m square card in the XY plane at z = 0 facing +Z, its texture repeated 96 times: about
/// 2.6 texels per pixel from 10 m, so the albedo is sampled a mip or more down.
fn card() -> LabMesh {
    let (half, reps) = (20.0, 96.0);
    let corners = [
        ([-half, -half, 0.0], [0.0, reps]),
        ([half, -half, 0.0], [reps, reps]),
        ([half, half, 0.0], [reps, 0.0]),
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

/// Renders `FRAMES` frames with the eye trucking `px_per_frame` pixels a frame along +X (the
/// card at one depth, so the whole card moves alike) and returns the last frame's linear
/// luminance over the measured square.
fn render(device: &RenderDevice, odd_alpha: u8, px_per_frame: f32) -> Vec<f32> {
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 12.0,
        bloom: false,
        taa: true,
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
    let mut renderer = FrameRenderer::new();
    renderer.set_taa_hashed_alpha(true);
    let metres_per_px = 2.0 * DISTANCE * (FOV_Y * 0.5).tan() / H as f32;
    let mut last = Vec::new();
    for frame in 0..FRAMES {
        let x = frame as f32 * px_per_frame * metres_per_px;
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
                albedo: Some(stripes(odd_alpha)),
                alpha_mode: ALPHA_MASK,
                alpha_cutoff: 0.5,
                double_sided: true,
                ..LabMaterial::default()
            }],
            meshes: vec![card()],
        };
        let mut scene = lab.as_frame_scene_with_quality(quality);
        scene.post = Some(&post);
        let image = renderer
            .render_offscreen(device, &scene)
            .unwrap_or_else(|err| panic!("frame {frame}: {err}"));
        last = image.pixels;
    }
    let (x0, y0) = ((W as usize - SIDE) / 2, (H as usize - SIDE) / 2);
    let mut luma = Vec::with_capacity(SIDE * SIDE);
    for y in y0..y0 + SIDE {
        for x in x0..x0 + SIDE {
            let i = (y * W as usize + x) * 4;
            let lin = |c: u8| {
                let c = f32::from(c) / 255.0;
                if c <= 0.040_45 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            luma.push(0.2126 * lin(last[i]) + 0.7152 * lin(last[i + 1]) + 0.0722 * lin(last[i + 2]));
        }
    }
    luma
}

fn mean(v: &[f32]) -> f32 {
    v.iter().sum::<f32>() / v.len() as f32
}

#[test]
fn a_moving_hashed_card_resolves_without_a_lattice_stipple() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let opaque = render(&device, 255, 0.37);
    let hashed = render(&device, 0, 0.37);
    let energy: StippleEnergy = stipple_energy(&hashed, SIDE, SIDE);
    let (m_opaque, m_hashed) = (mean(&opaque), mean(&hashed));
    eprintln!("hashed card, 0.37 px/frame: {energy:?}; mean luma {m_hashed:.4} (opaque card {m_opaque:.4})");
    assert!(m_opaque > 0.05, "the opaque card must be lit ({m_opaque})");
    // Half coverage: neither vanished nor filled (the resolve's inverse-luminance weights pull
    // the blend toward the dark side, so the band is wide).
    assert!(
        (0.1 * m_opaque..0.9 * m_opaque).contains(&m_hashed),
        "hashed card mean {m_hashed} vs opaque {m_opaque}"
    );
    assert!(
        energy.lattice < 0.08,
        "the resolved card keeps {:.1} % of its fine-scale power on the IGN lattice: {energy:?}",
        energy.lattice * 100.0
    );
    assert!(
        energy.checker < 0.08,
        "the resolved card keeps a (-1)^(x+y) checkerboard: {energy:?}"
    );
    assert!(
        energy.peak_ratio < 80.0,
        "one frequency dominates the resolved card (a lattice): {energy:?}"
    );
}

/// A held view keeps interleaved gradient noise (its evenly balanced neighbourhoods converge
/// best under the still history), so held frames draw what they drew before ADR-1489. The small
/// residual of a held, converged card still lies on the IGN lattice: that peak is the evidence
/// that the held flag (`interior_half.w` bit 5) reached the shader.
#[test]
fn a_held_hashed_card_keeps_the_interleaved_gradient_phase() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let held = stipple_energy(&render(&device, 0, 0.0), SIDE, SIDE);
    let moving = stipple_energy(&render(&device, 0, 0.37), SIDE, SIDE);
    eprintln!("hashed card held: {held:?}; moving: {moving:?}");
    assert!(
        held.peak_ratio > 150.0 && held.peak_ratio > 3.0 * moving.peak_ratio,
        "a held view should keep the IGN phase (its lattice peak): held {held:?}, moving {moving:?}"
    );
}
