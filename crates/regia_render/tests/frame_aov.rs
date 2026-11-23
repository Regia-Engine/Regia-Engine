// SPDX-License-Identifier: MPL-2.0
//! The frame's normal and roughness AOVs (ADR-2951, `regia_render::frame_aov`).
//!
//! A normal-mapped plane with a known roughness and metallic: the normal AOV must hold the mapped
//! normal (what the lights used, not the geometric one), and the roughness AOV the material's
//! roughness and metallic, within the 8-bit readback.

use regia_render::frame_aov::angular_error_deg;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabTexture,
    LabVertex, PostSettings, RenderDevice, capture_frame_aov,
};

const W: u32 = 128;
const H: u32 = 96;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A quad in the XY plane through the origin, front face +Z, tangent +X.
fn fin(half: f32) -> LabMesh {
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

/// A constant tangent-space normal (0.6, 0, 0.8).
fn tilted_normal_map() -> LabTexture {
    let px = [204u8, 128, 230, 255];
    LabTexture {
        width: 4,
        height: 4,
        rgba: px.iter().copied().cycle().take(4 * 4 * 4).collect(),
    }
}

fn scene(normal_map: bool) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.3, 0.2, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([-0.3, -0.4, -0.85], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.45, 0.3, 1.0],
            roughness: 0.35,
            metallic: 0.4,
            normal_scale: 1.0,
            normal_map: normal_map.then(tilted_normal_map),
            ..LabMaterial::default()
        }],
        meshes: vec![fin(1.4)],
    }
}

/// The centre 30 % of the image (inside the fin).
fn centre() -> Vec<bool> {
    (0..W * H)
        .map(|i| {
            let (x, y) = (i % W, i / W);
            (x as f32 / W as f32 - 0.5).abs() < 0.15 && (y as f32 / H as f32 - 0.5).abs() < 0.15
        })
        .collect()
}

#[test]
fn a_normal_mapped_plane_reads_back_its_shading_normal_roughness_and_metallic() {
    let device = device();
    let quality = FrameQuality {
        specular_aa: false,
        ..FrameQuality::LAB
    };
    for mapped in [false, true] {
        let lab = scene(mapped);
        let frame = lab.as_frame_scene_with_quality(quality);
        let mut renderer = FrameRenderer::new();
        let aov = capture_frame_aov(&mut renderer, &device, &frame).expect("frame AOV");
        assert_eq!((aov.width, aov.height), (W, H));
        let mask: Vec<bool> = centre()
            .iter()
            .zip(&aov.coverage)
            .map(|(a, b)| *a && *b)
            .collect();
        let covered = mask.iter().filter(|m| **m).count();
        assert!(covered > 500, "{covered} covered centre pixels");
        // The map's (0.6, 0.004, 0.804) in the frame (T = +X, N = +Z): 0.3° from (0.6, 0, 0.8).
        let want = if mapped { [0.6, 0.0, 0.8] } else { [0.0, 0.0, 1.0] };
        let (_, rms, n) = angular_error_deg(&aov.normal, &vec![want; aov.normal.len()], &mask);
        assert_eq!(n, covered);
        assert!(rms < 1.5, "mapped {mapped}: normal RMS {rms}° off {want:?}");
        for i in (0..mask.len()).filter(|&i| mask[i]) {
            assert!(
                (aov.roughness[i] - 0.35).abs() < 0.01,
                "roughness {}",
                aov.roughness[i]
            );
            assert!(
                (aov.metallic[i] - 0.4).abs() < 0.01,
                "metallic {}",
                aov.metallic[i]
            );
        }
        // Background (no geometry) is not covered: both corners sit outside the fin.
        assert!(!aov.coverage[0] && !aov.coverage[(W * H - 1) as usize]);
    }
}

#[test]
fn a_lite_tier_renderer_still_captures_the_debug_outputs() {
    // Mobile tiers on a tile GPU draw forward_mobile.wgsl, which has no debug outputs: without
    // the capture forcing the full shader, the AOV would decode lit colour as normals. The
    // renderer override stands in for a lite-accepting adapter on lavapipe.
    let device = device();
    let lab = scene(true);
    let frame = lab.as_frame_scene_with_quality(FrameQuality::LAB);
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(Some(regia_render::forward_lite::ForwardLite::Low));
    let aov = capture_frame_aov(&mut renderer, &device, &frame).expect("frame AOV");
    assert_eq!(
        renderer.forward_lite_override(),
        Some(regia_render::forward_lite::ForwardLite::Low),
        "the capture restores the renderer's lite tier"
    );
    let mask: Vec<bool> = centre()
        .iter()
        .zip(&aov.coverage)
        .map(|(a, b)| *a && *b)
        .collect();
    let covered = mask.iter().filter(|m| **m).count();
    assert!(covered > 500, "{covered} covered centre pixels");
    let (_, rms, n) = angular_error_deg(&aov.normal, &vec![[0.6, 0.0, 0.8]; aov.normal.len()], &mask);
    assert_eq!(n, covered);
    assert!(rms < 1.5, "lite tier: normal RMS {rms}° off");
    for i in (0..mask.len()).filter(|&i| mask[i]) {
        assert!(
            (aov.roughness[i] - 0.35).abs() < 0.01,
            "roughness {}",
            aov.roughness[i]
        );
        assert!(
            (aov.metallic[i] - 0.4).abs() < 0.01,
            "metallic {}",
            aov.metallic[i]
        );
    }
}

#[test]
fn the_capture_leaves_the_beauty_frame_unchanged() {
    let device = device();
    let lab = scene(true);
    let frame = lab.as_frame_scene_with_quality(FrameQuality::LAB);
    // Frames move on their own (frame-indexed noise): compare the same frame number, the fourth,
    // of a renderer that never captured.
    let mut fresh = FrameRenderer::new();
    let mut want = Vec::new();
    for _ in 0..4 {
        want = fresh.render_offscreen(&device, &frame).expect("frame").pixels;
    }
    let mut captured = FrameRenderer::new();
    let _ = captured.render_offscreen(&device, &frame).expect("frame");
    capture_frame_aov(&mut captured, &device, &frame).expect("frame AOV");
    assert!(captured.translucent_layers().is_empty());
    let got = captured.render_offscreen(&device, &frame).expect("frame").pixels;
    assert_eq!(
        got, want,
        "a beauty frame after the capture differs from one without it"
    );
}
