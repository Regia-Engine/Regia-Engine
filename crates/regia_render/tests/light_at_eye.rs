// SPDX-License-Identifier: MPL-2.0
//! A local light exactly at the camera (a headlight, a flashlight) prints no black pixels.
//!
//! With the light at the eye, `l == v`, so the half vector is `v` and V·H rounds to 1 + 1 ulp on
//! some pixels. Schlick's `pow(1 − V·H, 5)` then had a negative base, which WGSL leaves
//! undefined (NaN on lavapipe), and the NaN printed black: a speckle over the whole cone (about
//! 5 % of the frame for a spot, 11 % for a point light, in the tunnel acceptance scene).

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabLocalLight, LabMaterial, LabMesh, LabPointLight,
    LabScene, LabSpotLight, LabVertex, PostSettings, RenderDevice,
};

const W: u32 = 160;
const H: u32 = 96;
const EYE: [f32; 3] = [0.0, 1.6, 0.0];
const TARGET: [f32; 3] = [0.0, 0.4, -6.0];

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip light at eye: {err}");
            None
        }
    }
}

fn quad(corners: [[f32; 3]; 4], normal: [f32; 3]) -> LabMesh {
    LabMesh {
        vertices: corners
            .iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(*p, normal, uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        material: 0,
        ..LabMesh::unskinned()
    }
}

fn render(light: Option<LabLocalLight>, quality: FrameQuality) -> Vec<u8> {
    let device = device().expect("caller checks the device");
    let post = PostSettings {
        exposure_ev: 6.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: 1.0,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 6.0,
        ambient_lux: 5.0,
        double_sided: true,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.2, -1.0, 0.15], [1.0, 1.0, 1.0], 0.0),
        local_lights: light.into_iter().collect(),
        materials: vec![LabMaterial {
            base_color: [0.5, 0.5, 0.5, 1.0],
            roughness: 0.6,
            ..LabMaterial::default()
        }],
        meshes: vec![
            // Floor and a wall facing the camera.
            quad(
                [
                    [-20.0, 0.0, 5.0],
                    [20.0, 0.0, 5.0],
                    [20.0, 0.0, -12.0],
                    [-20.0, 0.0, -12.0],
                ],
                [0.0, 1.0, 0.0],
            ),
            quad(
                [
                    [-20.0, 0.0, -12.0],
                    [20.0, 0.0, -12.0],
                    [20.0, 8.0, -12.0],
                    [-20.0, 8.0, -12.0],
                ],
                [0.0, 0.0, 1.0],
            ),
        ],
    };
    FrameRenderer::new()
        .render_offscreen(&device, &scene.as_frame_scene_with_quality(quality))
        .expect("render")
        .pixels
}

/// Pixels black (all channels under 3) with the light on that are not black without it: adding
/// light can only brighten a pixel, so these are NaN.
fn blackened_pixels(lit: &[u8], unlit: &[u8]) -> usize {
    let black = |p: &[u8; 4]| p[0] < 3 && p[1] < 3 && p[2] < 3;
    lit.as_chunks::<4>()
        .0
        .iter()
        .zip(unlit.as_chunks::<4>().0)
        .filter(|(l, u)| black(l) && !black(u))
        .count()
}

fn mean(px: &[u8]) -> f32 {
    px.as_chunks::<4>()
        .0
        .iter()
        .map(|p| (f32::from(p[0]) + f32::from(p[1]) + f32::from(p[2])) / 3.0)
        .sum::<f32>()
        / (px.len() / 4) as f32
}

#[test]
fn a_light_at_the_eye_prints_no_black_pixels() {
    if device().is_none() {
        return;
    }
    let dir = {
        let d = [TARGET[0] - EYE[0], TARGET[1] - EYE[1], TARGET[2] - EYE[2]];
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        [d[0] / len, d[1] / len, d[2] / len]
    };
    let lights = [
        (
            "point",
            LabLocalLight::Point(LabPointLight {
                position: EYE,
                color: [1.0, 0.95, 0.85],
                intensity: 20_000.0,
                radius: 40.0,
                cast_shadows: false,
                source_radius: None,
            }),
        ),
        (
            "spot",
            LabLocalLight::Spot(LabSpotLight {
                position: EYE,
                direction: dir,
                color: [1.0, 0.95, 0.85],
                intensity: 4_000.0,
                range: 40.0,
                inner_angle: 0.15,
                outer_angle: 0.45,
                cast_shadows: false,
                source_radius: None,
            }),
        ),
    ];
    let mut desktop = regia_render::FrameQuality::from_profile(&regia_render::RenderProfile::from_preset(
        regia_render::RenderProfilePreset::DesktopHigh,
    ));
    desktop.taau = false;
    for (quality_name, quality) in [("lab", FrameQuality::LAB), ("desktop-high", desktop)] {
        let dark = render(None, quality);
        for (name, light) in lights {
            let lit = render(Some(light), quality);
            let (black, lit_mean, dark_mean) = (blackened_pixels(&lit, &dark), mean(&lit), mean(&dark));
            eprintln!(
                "{quality_name} {name} light at the eye: {black} black pixels of {}, mean {lit_mean:.1} (unlit {dark_mean:.1})",
                W * H
            );
            assert!(
                lit_mean > dark_mean + 10.0,
                "{quality_name} {name}: the light must light the frame ({lit_mean:.1} vs {dark_mean:.1})"
            );
            assert_eq!(
                black, 0,
                "{quality_name} {name}: a light at the eye printed {black} black (NaN) pixels"
            );
        }
    }
}
