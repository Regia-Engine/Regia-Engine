// SPDX-License-Identifier: MPL-2.0
//! Temporal upscaler: a thin feature stays thin, and a moving quad does not leave a ghost.

use glam::{Mat4, Vec3};
use regia_render::{
    FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex, PostSettings, RenderDevice,
    TonemapOperator,
};
use regia_test_utils::golden::{DEFAULT_SSIM_THRESHOLD, compare_rgba8_to_png, golden_path};

const W: u32 = 96;
const H: u32 = 96;

fn vert(position: [f32; 3]) -> LabVertex {
    LabVertex {
        position,
        normal: [0.0, 0.0, 1.0],
        uv: [0.0, 0.0],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    }
}

fn quad(x0: f32, x1: f32, y0: f32, y1: f32) -> LabMesh {
    LabMesh {
        vertices: vec![
            vert([x0, y0, 0.0]),
            vert([x1, y0, 0.0]),
            vert([x1, y1, 0.0]),
            vert([x0, y1, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

fn taau_quality() -> FrameQuality {
    let mut quality = FrameQuality::LAB;
    quality.render_scale_permille = 700;
    quality.taau = true;
    quality
}

fn post() -> PostSettings {
    PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        exposure_ev: 0.0,
        tonemap: TonemapOperator::AgX,
        fxaa: false,
        taa: false,
        film_grain: 0.0,
        vignette: 0.0,
        ..PostSettings::default()
    }
}

fn scene_with(mesh: LabMesh, settings: PostSettings) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 3.2],
        target: [0.0, 0.0, 0.0],
        fov_y: 1.0,
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: Some(settings),
        directional: regia_render::DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [1.0, 1.0, 1.0, 1.0],
            unlit: true,
            ..LabMaterial::default()
        }],
        meshes: vec![mesh],
    }
}

fn column_luma(pixels: &[u8], x: u32) -> f32 {
    let mut sum = 0.0f32;
    for y in 0..H {
        let i = ((y * W + x) * 4) as usize;
        sum += 0.2126 * pixels[i] as f32 + 0.7152 * pixels[i + 1] as f32 + 0.0722 * pixels[i + 2] as f32;
    }
    sum / H as f32
}

fn mean_abs_rect(a: &[u8], b: &[u8], x0: u32, x1: u32) -> f32 {
    let mut sum = 0u64;
    let mut n = 0u64;
    for y in 0..H {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            for channel in 0..3 {
                sum += u64::from(a[i + channel].abs_diff(b[i + channel]));
            }
            n += 3;
        }
    }
    sum as f32 / n.max(1) as f32
}

#[test]
fn taau_keeps_a_thin_feature_and_rejects_ghosts() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip taau_keeps_a_thin_feature_and_rejects_ghosts: {err}");
            return;
        }
    };
    let settings = post();
    let quality = taau_quality();
    let mut renderer = FrameRenderer::new();
    // About three pixels wide at 70% of 96×96 (a sub-pixel line misses every sample).
    let thin = scene_with(quad(-0.08, 0.08, -0.85, 0.85), settings.clone());
    let mut last = Vec::new();
    for _ in 0..8 {
        last = renderer
            .render_offscreen(&device, &thin.as_frame_scene_with_quality(quality))
            .expect("thin frame")
            .pixels;
    }
    let columns: Vec<f32> = (0..W).map(|x| column_luma(&last, x)).collect();
    let (peak_x, peak) = columns
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, v)| (i as u32, *v))
        .expect("columns");
    assert!(peak > 80.0, "thin feature must stay bright, peak {peak:.1}");
    let far = columns
        .iter()
        .enumerate()
        .filter(|(i, _)| (*i as i32 - peak_x as i32).abs() > 8)
        .map(|(_, v)| *v)
        .fold(0.0f32, f32::max);
    assert!(
        far < 40.0,
        "thin feature smeared (peak col {peak_x} luma {peak:.1}, far luma {far:.1})"
    );

    // History at the left, then a jump to the right. The clamp must drop the old quad.
    let mut moving = scene_with(quad(-0.18, 0.18, -0.45, 0.45), settings.clone());
    let mut taau = FrameRenderer::new();
    let mut native_q = FrameQuality::LAB;
    native_q.taau = false;
    native_q.render_scale_permille = 1000;
    let mut native = FrameRenderer::new();
    for step in 0..6 {
        let x = if step < 3 { -1.15 } else { 1.15 };
        moving.meshes[0].model = Mat4::from_translation(Vec3::new(x, 0.0, 0.0)).to_cols_array_2d();
        let taau_px = taau
            .render_offscreen(&device, &moving.as_frame_scene_with_quality(quality))
            .expect("taau move");
        if step == 5 {
            // Native control at the same pose, one frame, no history.
            let control = native
                .render_offscreen(&device, &moving.as_frame_scene_with_quality(native_q))
                .expect("control")
                .pixels;
            let ghost = mean_abs_rect(&taau_px.pixels, &control, 0, W / 5);
            assert!(
                ghost < 12.0,
                "vacated region ghosted: mean abs {ghost:.2} (threshold 12)"
            );
            let right = (W / 2..W)
                .map(|x| column_luma(&taau_px.pixels, x))
                .fold(0.0, f32::max);
            assert!(right > 40.0, "moving quad left the right half, luma {right:.1}");
        }
    }

    // Same frame count and the same last jitter. History-on has accumulated the
    // thin line; the other renderer upscales only its current frame (history
    // was invalid). A history-off resolve makes these two images match, so the
    // difference itself is the rejection check.
    let mut accumulated = FrameRenderer::new();
    let mut current_only = FrameRenderer::new();
    let mut cold = quality;
    cold.taau = false;
    let mut acc_px = Vec::new();
    let mut cur_px = Vec::new();
    for frame in 0..8 {
        acc_px = accumulated
            .render_offscreen(&device, &thin.as_frame_scene_with_quality(quality))
            .expect("history frame")
            .pixels;
        let q = if frame == 7 { quality } else { cold };
        cur_px = current_only
            .render_offscreen(&device, &thin.as_frame_scene_with_quality(q))
            .expect("current-only frame")
            .pixels;
    }
    // Measured over the columns around the line: the feature is ~3 % of the frame, and a
    // full-frame mean was diluted by empty background. (It only cleared the threshold
    // before because the resolve's YCoCg inverse doubled Co every frame, so the history
    // drifted the dark clear colour toward blue — the bug fixed alongside this change.)
    // A history-off resolve gives exactly 0. The threshold was 1.0 while the previous
    // view-projection was stored jittered: a held history was refetched up to half a pixel
    // off every frame (a random-walk blur, fixed by the motion-look lane), and that blur was
    // most of the difference. A resolve that reuses its history exactly measures 0.68 here
    // (accumulating bilinear upsamples of jittered frames stays close to one of them).
    let history_delta = mean_abs_rect(&acc_px, &cur_px, W / 2 - 8, W / 2 + 8);
    assert!(
        history_delta > 0.3,
        "history rejection did not change the thin feature (mean abs {history_delta:.2}; a history-off upscale matches the accumulated frame)"
    );

    compare_rgba8_to_png(&last, W, H, &golden_path("taau_thin"), DEFAULT_SSIM_THRESHOLD)
        .expect("taau thin golden");
}
