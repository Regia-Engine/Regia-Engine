// SPDX-License-Identifier: MPL-2.0
//! A held camera must not accumulate haze (motion-look lane).
//!
//! Desktop High reuses the SSGI/SSR composite while the view is static (the
//! static-screen cache). The outdoor composite — LUT sky, aerial perspective, clouds,
//! froxel fog — blended into that cached image, so every held frame added its haze on
//! top of the last frame's: on DO27's Switchbacks a canyon wall behind ground haze went
//! from 126 to 191 levels in 40 frames, and dropped back the frame the camera moved
//! (a 130-level pop when a chase started).

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, TimeOfDay,
    VolumetricFogSettings,
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

fn scene() -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.8, 0.0],
        target: [0.0, 6.0, -60.0],
        fov_y: 0.9,
        near: 0.1,
        far: 2_000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.3, -0.6, -0.4], [1.0, 0.95, 0.9], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.4, 0.3, 0.25, 1.0],
            roughness: 0.9,
            ..LabMaterial::default()
        }],
        meshes: vec![
            // Ground.
            quad(
                [
                    [-200.0, 0.0, 20.0],
                    [200.0, 0.0, 20.0],
                    [200.0, 0.0, -200.0],
                    [-200.0, 0.0, -200.0],
                ],
                [0.0, 1.0, 0.0],
                0,
            ),
            // A wall 120 m away, behind the haze.
            quad(
                [
                    [-150.0, 0.0, -120.0],
                    [150.0, 0.0, -120.0],
                    [150.0, 60.0, -120.0],
                    [-150.0, 60.0, -120.0],
                ],
                [0.0, 0.0, 1.0],
                0,
            ),
        ],
    }
}

fn wall_luma(pixels: &[u8]) -> f32 {
    // The middle of the frame: the wall under the haze.
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in H / 4..H / 2 {
        for x in W / 4..3 * W / 4 {
            let i = ((y * W + x) * 4) as usize;
            sum += 0.2126 * f32::from(pixels[i])
                + 0.7152 * f32::from(pixels[i + 1])
                + 0.0722 * f32::from(pixels[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

#[test]
fn held_frames_do_not_accumulate_haze() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    assert!(
        quality.reuse_static_screen(),
        "this test covers the static-screen cache (SSGI + paged sun shadows)"
    );
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        ..PostSettings::golden_deterministic()
    };
    let lab = scene();
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(OutdoorParams {
        time: Some(TimeOfDay::noon()),
        fog: VolumetricFogSettings {
            density: 0.01,
            height_falloff: 0.05,
            ..VolumetricFogSettings::default()
        },
        ..OutdoorParams::default()
    });
    let mut lumas = Vec::new();
    for _ in 0..24 {
        let image = renderer.render_offscreen(&device, &frame).expect("held frame");
        lumas.push(wall_luma(&image.pixels));
    }
    eprintln!("wall luma over held frames: {lumas:?}");
    // A held frame is the first frame again: the fog history reprojects onto itself, and
    // the haze is blended once per frame, not once more per frame held. (Before the fix
    // the wall climbed 165.8 -> 192.3 levels and settled there.)
    let first = lumas[0];
    let last = lumas[23];
    assert!(
        (last - first).abs() <= 1.5,
        "the hazed wall drifted from {first:.1} to {last:.1} levels over 23 held frames"
    );
}
