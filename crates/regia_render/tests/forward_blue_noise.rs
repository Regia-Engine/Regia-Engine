// SPDX-License-Identifier: MPL-2.0
//! Blue noise in the forward pass (ADR-2075 remainder, ADR-2652; lavapipe, no goldens).
//!
//! With `FrameQuality::blue_noise` on, the forward shader's stochastic terms (the sun and local
//! shadow kernel rotation, the hashed alpha threshold, the pane SSR start offset) read the
//! spatiotemporal blue-noise table bound in the frame's view group, where they used interleaved
//! gradient noise. Off, nothing changes.
//!
//! A white masked card whose every mip below the top holds alpha 0.5 fills the view under TAA
//! with the hashed alpha test: the prepass discards exactly the pixels whose threshold is above
//! the alpha, so the scene depth is the hashed-alpha mask. With blue noise on that mask is the
//! CPU table thresholded at the card's alpha (for one slice), and it passes the table's own
//! spectrum check (`blue_noise::low_frequency_share`, as `every_slice_has_a_blue_spectrum`).
//! Off, the mask is the interleaved-gradient one (far from the table); default frames are pinned
//! bit for bit by `studio_frame_pin`.

use regia_render::blue_noise::{SIZE, SLICES, low_frequency_share, scalar};
use regia_render::{
    ALPHA_MASK, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    LabTexture, LabVertex, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 160;
const H: u32 = 120;
const FOV_Y: f32 = 0.8;
const DISTANCE: f32 = 10.0;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// 16 × 16 white, alpha 255 / 0 by column: every mip below the top averages to alpha 0.5.
fn stripes() -> LabTexture {
    let mut rgba = Vec::with_capacity(16 * 16 * 4);
    for _row in 0..16 {
        for col in 0..16 {
            rgba.extend_from_slice(&[255, 255, 255, if col % 2 == 0 { 255 } else { 0 }]);
        }
    }
    LabTexture {
        width: 16,
        height: 16,
        rgba,
    }
}

/// A 40 m card at z = 0 facing +Z, its texture repeated 96 times (a mip or more down from 10 m).
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

fn lab() -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, DISTANCE],
        target: [0.0, 0.0, 0.0],
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
            albedo: Some(stripes()),
            alpha_mode: ALPHA_MASK,
            alpha_cutoff: 0.5,
            double_sided: true,
            ..LabMaterial::default()
        }],
        meshes: vec![card()],
    }
}

fn quality(blue_noise: bool) -> FrameQuality {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.render_scale_permille = 1000;
    quality.gpu_driven = false;
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.volumetric_fog = false;
    quality.volumetric_clouds = false;
    quality.gi_probes = false;
    quality.blue_noise = blue_noise;
    quality
}

/// One frame; returns its pixels and the hashed-alpha mask (1 = the card kept the pixel).
fn frame(renderer: &mut FrameRenderer, device: &RenderDevice, blue_noise: bool) -> (Vec<u8>, Vec<u8>) {
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 12.0,
        bloom: false,
        taa: true,
        ..PostSettings::golden_deterministic()
    };
    let lab = lab();
    let mut scene = lab.as_frame_scene_with_quality(quality(blue_noise));
    scene.post = Some(&post);
    let pixels = renderer.render_offscreen(device, &scene).expect("frame").pixels;
    let depth = renderer.read_scene_depth(device, W, H).expect("scene depth");
    (pixels, depth.iter().map(|&d| u8::from(d < 1.0)).collect())
}

/// The best agreement between `mask` (a `SIZE`² window at the frame's origin) and the CPU
/// table thresholded at `alpha` over the slices: `(slice, share of pixels that agree)`.
fn best_slice(mask: &[u8], x0: u32, y0: u32, alpha: f32) -> (u32, f64) {
    (0..SLICES as u32)
        .map(|z| {
            let mut agree = 0usize;
            for y in 0..SIZE as u32 {
                for x in 0..SIZE as u32 {
                    let (px, py) = (x0 + x, y0 + y);
                    // Kept where the alpha passes the threshold (`alpha_rejects`: alpha < noise).
                    let kept = alpha >= scalar(px, py, z);
                    if kept == (mask[(py * W + px) as usize] == 1) {
                        agree += 1;
                    }
                }
            }
            (z, agree as f64 / (SIZE * SIZE) as f64)
        })
        .fold((0, 0.0), |a, b| if b.1 > a.1 { b } else { a })
}

#[test]
fn the_forward_hashed_alpha_mask_is_the_blue_noise_table_and_blue() {
    let device = device();
    let mut renderer = FrameRenderer::new();
    renderer.set_taa_hashed_alpha(true);
    // Off: the plain frame. Twice, so the held-view phase is the one compared below.
    frame(&mut renderer, &device, false);
    let (off_pixels, off_mask) = frame(&mut renderer, &device, false);
    // On: the mask reads the table.
    let mut on_mask = Vec::new();
    for _ in 0..3 {
        on_mask = frame(&mut renderer, &device, true).1;
    }
    let (x0, y0) = ((W - SIZE as u32) / 2, (H - SIZE as u32) / 2);
    let kept = on_mask.iter().filter(|&&m| m == 1).count() as f64 / on_mask.len() as f64;
    // The card's alpha at its mips: 127 or 128 / 255.
    let (slice, agree) = [127.0f32 / 255.0, 128.0 / 255.0]
        .iter()
        .map(|&a| best_slice(&on_mask, x0, y0, a))
        .fold((0, 0.0), |a, b| if b.1 > a.1 { b } else { a });
    let (_, agree_off) = [127.0f32 / 255.0, 128.0 / 255.0]
        .iter()
        .map(|&a| best_slice(&off_mask, x0, y0, a))
        .fold((0, 0.0), |a, b| if b.1 > a.1 { b } else { a });
    let window = |mask: &[u8]| -> Vec<u8> {
        (0..SIZE as u32)
            .flat_map(|y| (0..SIZE as u32).map(move |x| ((y0 + y) * W + x0 + x) as usize))
            .map(|i| mask[i] * 255)
            .collect()
    };
    let share_on = low_frequency_share(&window(&on_mask), SIZE, 0.3);
    let share_off = low_frequency_share(&window(&off_mask), SIZE, 0.3);
    println!(
        "kept {:.3}; on: slice {slice} agrees {:.4}, low share {share_on:.4}; off: best slice agrees {agree_off:.4}, \
         low share {share_off:.4}",
        kept, agree
    );
    assert!((0.4..0.6).contains(&kept), "about half the card is kept: {kept}");
    assert!(
        agree > 0.99,
        "the mask is the table thresholded: {agree} (slice {slice})"
    );
    assert!(agree_off < 0.9, "off, the mask is not the table: {agree_off}");
    // ADR-2652's pre-registered bound: a third of white noise's 7 % below 0.3 of Nyquist.
    assert!(share_on < 0.025, "the mask's low-frequency share {share_on}");

    // Off frames are pinned by `studio_frame_pin` (the default frame composes `FRAME_BLUE_NOISE`
    // on this device and binds the stand-in): its hashes did not move.
    assert!(!off_pixels.is_empty());
}
