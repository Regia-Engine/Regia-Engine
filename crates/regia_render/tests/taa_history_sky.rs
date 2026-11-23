// SPDX-License-Identifier: MPL-2.0
//! One-shot Desktop High TAA (temporal upscale from 0.67) must keep the sky.
//! A history-less resolve that clears to black and then rejects the current
//! colour paints the background black while the ground stays lit — the DO27
//! capture bug.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset,
};

fn ground() -> LabScene {
    let vertices = [
        LabVertex::from_pos_normal_uv([-8.0, 0.0, -8.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([8.0, 0.0, -8.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([8.0, 0.0, 8.0], [0.0, 1.0, 0.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([-8.0, 0.0, 8.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
    ];
    LabScene {
        width: 160,
        height: 90,
        eye: [0.0, 1.6, 6.0],
        target: [0.0, 1.2, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.85, -0.3], [1.0, 0.97, 0.9], 110_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.35, 0.42, 0.22, 1.0],
            roughness: 0.85,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: vertices.to_vec(),
            indices: vec![0, 2, 1, 0, 3, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

fn pixel(pixels: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * w + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn one_shot_native_taa_keeps_the_sky() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: true,
        ..PostSettings::golden_deterministic()
    };
    let mut scene = ground();
    scene.post = Some(post.clone());
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    frame.ibl = true;
    let mut renderer = FrameRenderer::new();
    let image = renderer
        .render_offscreen(&device, &frame)
        .expect("one-shot TAA frame");
    let sky = pixel(&image.pixels, image.width, image.width / 2, 4);
    let ground_px = pixel(&image.pixels, image.width, image.width / 2, image.height - 8);
    eprintln!(
        "sky {sky:?} ground {ground_px:?} cpu {:?}",
        renderer.cpu_timings()
    );
    assert!(
        sky[2] > sky[0] + 15 && sky[2] > 40,
        "one-shot TAA sky is black or not blue: {sky:?} (ground {ground_px:?})"
    );
}

fn region_mean(pixels: &[u8], w: u32, h: u32, x: u32, y: u32, radius: u32) -> [f32; 3] {
    let mut sum = [0u32; 3];
    let mut n = 0u32;
    let x0 = x.saturating_sub(radius);
    let y0 = y.saturating_sub(radius);
    let x1 = (x + radius).min(w - 1);
    let y1 = (y + radius).min(h - 1);
    for py in y0..=y1 {
        for px in x0..=x1 {
            let p = pixel(pixels, w, px, py);
            sum[0] += u32::from(p[0]);
            sum[1] += u32::from(p[1]);
            sum[2] += u32::from(p[2]);
            n += 1;
        }
    }
    let n = n.max(1) as f32;
    [sum[0] as f32 / n, sum[1] as f32 / n, sum[2] as f32 / n]
}

/// Native-scale TAA (Ultra's scale 1, screen-space passes off) must copy the
/// current frame when history is invalid. A 0.9 blend against a black history
/// pulls the sky well outside the 12-level band around the non-TAA reference.
#[test]
fn one_shot_scale_one_taa_matches_the_current_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopUltra));
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.ssao = regia_render::SsaoMode::Off;
    quality.gi = regia_render::GiMode::Ambient;
    quality.reflection_probes = false;
    assert_eq!(quality.render_scale_permille, 1000, "this test is native scale");
    assert!(quality.taau);
    assert!(!quality.ssgi_half_res());
    assert_eq!(quality.pcss_taps(), (12, 16));

    let post_taa = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: true,
        ..PostSettings::golden_deterministic()
    };
    let mut post_ref = post_taa.clone();
    post_ref.taa = false;

    let mut scene = ground();
    scene.post = Some(post_taa.clone());
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post_taa);
    frame.ibl = true;
    let mut taa_renderer = FrameRenderer::new();
    let taa = taa_renderer
        .render_offscreen(&device, &frame)
        .expect("native one-shot TAA");

    let mut reference_quality = quality;
    reference_quality.taau = false;
    scene.post = Some(post_ref.clone());
    let mut reference_frame = scene.as_frame_scene_with_quality(reference_quality);
    reference_frame.post = Some(&post_ref);
    reference_frame.ibl = true;
    let mut reference_renderer = FrameRenderer::new();
    let reference = reference_renderer
        .render_offscreen(&device, &reference_frame)
        .expect("non-TAA reference");

    let sky_t = region_mean(&taa.pixels, taa.width, taa.height, taa.width / 2, 4, 2);
    let sky_r = region_mean(
        &reference.pixels,
        reference.width,
        reference.height,
        reference.width / 2,
        4,
        2,
    );
    let ground_y = taa.height.saturating_sub(12);
    let ground_t = region_mean(&taa.pixels, taa.width, taa.height, taa.width / 2, ground_y, 2);
    let ground_r = region_mean(
        &reference.pixels,
        reference.width,
        reference.height,
        reference.width / 2,
        ground_y,
        2,
    );
    eprintln!("native sky taa {sky_t:?} ref {sky_r:?} ground taa {ground_t:?} ref {ground_r:?}");
    for (name, got, expected) in [("sky", sky_t, sky_r), ("ground", ground_t, ground_r)] {
        for channel in 0..3 {
            let delta = (got[channel] - expected[channel]).abs();
            assert!(
                delta <= 12.0,
                "scale-1 frame 0 {name} channel {channel} differs by {delta:.1} (taa {got:?}, current frame {expected:?}) — a black history blend fails this"
            );
        }
    }
    assert!(sky_t[2] > 40.0, "scale-1 one-shot sky is not blue: {sky_t:?}");
}
