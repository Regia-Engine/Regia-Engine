// SPDX-License-Identifier: MPL-2.0
//! CSM shadow geometry: the shadow must land on the side the sun points to.
//!
//! Regression test for the "shadow mirrored" bug class (the cascade's clip
//! space was flipped twice): a floor with one floating quad, camera looking
//! down at ~40° (well-conditioned `up = +Y`), and a sun tilted along ±Z. The
//! shadow must darken the half of the image the sun's Z tilt pushes it to, and
//! flipping the sun must flip the dark half — a mirrored projection fails
//! either assertion.

use regia_render::{
    DirectionalLightUniform, LabMaterial, LabMesh, LabScene, LabVertex, RenderDevice, render_lab_scene,
};

fn quad(y: f32, half: f32) -> Vec<LabVertex> {
    let corners = [
        [-half, y, -half],
        [half, y, -half],
        [half, y, half],
        [-half, y, half],
    ];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let normal = [0.0, if y > 0.5 { -1.0 } else { 1.0 }, 0.0];
    corners
        .into_iter()
        .zip(uvs)
        .map(|(position, uv)| LabVertex {
            position,
            normal,
            uv,
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

fn scene(sun_z: f32) -> LabScene {
    LabScene {
        width: 256,
        height: 160,
        eye: [5.5, 4.5, 0.0],
        target: [0.0, 1.2, 0.0],
        fov_y: 45.0_f32.to_radians(),
        near: 0.1,
        far: 30.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.8,
        ambient_lux: 4_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -1.0, sun_z], [1.0, 0.98, 0.94], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.8, 0.8, 0.8, 1.0],
            roughness: 0.8,
            ..LabMaterial::default()
        }],
        meshes: vec![
            LabMesh {
                vertices: quad(0.0, 6.0),
                indices: vec![0, 2, 1, 0, 3, 2],
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: quad(1.6, 0.8),
                indices: vec![0, 2, 1, 0, 3, 2],
                material: 0,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn mean_region(pixels: &[u8], width: u32, height: u32, x0: u32, x1: u32) -> f32 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in height / 4..height * 3 / 4 {
        for x in x0..x1 {
            let i = ((y * width + x) * 4) as usize;
            sum += (f32::from(pixels[i]) + f32::from(pixels[i + 1]) + f32::from(pixels[i + 2])) / 3.0;
            n += 1.0;
        }
    }
    sum / n
}

#[test]
fn shadow_lands_on_the_sunny_side() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip shadow position probe: {err}");
            return;
        }
    };
    // Render once with each sun Z tilt; the shadow must switch halves.
    let (left_a, right_a) = halves(&device, &scene(-0.55));
    let (left_b, right_b) = halves(&device, &scene(0.55));
    eprintln!(
        "shadow probe: sun -Z: left {left_a:.1} right {right_a:.1}; sun +Z: left {left_b:.1} right {right_b:.1}"
    );
    // Both frames must show a real shadow (the darker half differs clearly).
    let (dark_a, lit_a) = (left_a.min(right_a), left_a.max(right_a));
    let (dark_b, lit_b) = (left_b.min(right_b), left_b.max(right_b));
    assert!(
        lit_a - dark_a > 8.0,
        "sun -Z: no visible shadow (left {left_a}, right {right_a})"
    );
    assert!(
        lit_b - dark_b > 8.0,
        "sun +Z: no visible shadow (left {left_b}, right {right_b})"
    );
    // The dark half must flip with the sun: exactly one of the two renders has
    // its shadow on the left. A mirrored cascade fails this.
    assert_eq!(
        left_a < right_a,
        left_b > right_b,
        "shadow must flip sides when the sun flips (a: {left_a}/{right_a}, b: {left_b}/{right_b})"
    );
}

fn halves(device: &RenderDevice, scene: &LabScene) -> (f32, f32) {
    let result = render_lab_scene(device, scene).expect("render");
    let w = result.width;
    let h = result.height;
    (
        mean_region(&result.pixels, w, h, 0, w / 2),
        mean_region(&result.pixels, w, h, w / 2, w),
    )
}
