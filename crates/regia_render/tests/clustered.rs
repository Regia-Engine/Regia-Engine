// SPDX-License-Identifier: MPL-2.0
//! Clustered point and spot lights on the lab frame.

use regia_render::{
    DirectionalLightUniform, LabLocalLight, LabMaterial, LabMesh, LabPointLight, LabScene, LabSpotLight,
    RenderDevice, render_lab_scene, uv_sphere,
};

fn sphere_at(center: [f32; 3]) -> LabMesh {
    let (verts, indices) = uv_sphere(0.45, 16, 12);
    let vertices = verts
        .iter()
        .map(|v| {
            let mut lab = *v;
            lab.position[0] += center[0];
            lab.position[1] += center[1];
            lab.position[2] += center[2];
            lab
        })
        .collect();
    LabMesh {
        vertices,
        indices,
        material: 0,
        ..LabMesh::unskinned()
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn clustered_point_and_spot_change_colour() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip clustered lights: {err}");
            return;
        }
    };
    let scene = LabScene {
        width: 128,
        height: 128,
        eye: [0.0, 1.4, 4.2],
        target: [0.0, 0.4, 0.0],
        fov_y: 50.0_f32.to_radians(),
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 4.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -1.0, 0.1], [1.0, 1.0, 1.0], 0.15),
        local_lights: vec![
            LabLocalLight::Point(LabPointLight {
                position: [-0.9, 1.6, 1.2],
                color: [1.0, 0.05, 0.05],
                intensity: 800.0,
                radius: 6.0,
                cast_shadows: false,
                source_radius: None,
            }),
            LabLocalLight::Spot(LabSpotLight {
                position: [1.8, 2.2, 1.6],
                direction: [-0.55, -0.75, -0.35],
                color: [0.05, 1.0, 0.15],
                intensity: 90.0,
                range: 8.0,
                inner_angle: 0.3,
                outer_angle: 0.7,
                cast_shadows: false,
                source_radius: None,
            }),
        ],
        materials: vec![LabMaterial {
            base_color: [0.85, 0.85, 0.85, 1.0],
            roughness: 0.4,
            ..LabMaterial::default()
        }],
        meshes: vec![sphere_at([-0.85, 0.45, 0.0]), sphere_at([0.85, 0.45, 0.0])],
    };
    let frame = render_lab_scene(&device, &scene).expect("render");
    assert_eq!(frame.pixels.len(), (128 * 128 * 4) as usize);
    let mut left_r = 0u8;
    let mut right_g = 0u8;
    for y in 0..128 {
        for x in 0..128 {
            let i = ((y * 128 + x) * 4) as usize;
            if x < 64 {
                left_r = left_r.max(frame.pixels[i]);
            } else {
                right_g = right_g.max(frame.pixels[i + 1]);
            }
        }
    }
    assert!(
        left_r > 40,
        "left side should catch the red point light, max r={left_r}"
    );
    assert!(
        right_g > 40,
        "right side should catch the green spot, max g={right_g}"
    );
}

/// A light whose reach covers only its own corner of the screen still lights what it reaches.
/// The froxel lookup took the fragment's framebuffer position for NDC, so every fragment read
/// the bottom-right column's list: a light lit nothing unless its sphere also reached that
/// corner (a cockpit lamp at the eye does; a lamp under a parked hull 16 m out did not).
#[test]
fn a_small_light_away_from_the_screen_corner_lights_its_surface() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip clustered lights: {err}");
            return;
        }
    };
    let scene = |intensity: f32| LabScene {
        width: 128,
        height: 128,
        eye: [0.0, 1.0, 8.0],
        target: [0.0, 0.5, 0.0],
        fov_y: 50.0_f32.to_radians(),
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 4.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -1.0, 0.1], [1.0, 1.0, 1.0], 0.15),
        // 1.2 m of reach around a sphere on the left: nowhere near the bottom-right froxels.
        local_lights: vec![LabLocalLight::Point(LabPointLight {
            position: [-2.5, 0.9, 0.8],
            color: [1.0, 0.05, 0.05],
            intensity,
            radius: 1.2,
            cast_shadows: false,
            source_radius: None,
        })],
        materials: vec![LabMaterial {
            base_color: [0.85, 0.85, 0.85, 1.0],
            roughness: 0.4,
            ..LabMaterial::default()
        }],
        meshes: vec![sphere_at([-2.5, 0.45, 0.0])],
    };
    let red = |intensity: f32| {
        let frame = render_lab_scene(&device, &scene(intensity)).expect("render");
        let mut max_r = 0u8;
        for y in 0..128 {
            for x in 0..64 {
                let i = ((y * 128 + x) * 4) as usize;
                max_r = max_r.max(frame.pixels[i]);
            }
        }
        max_r
    };
    let (dark, lit) = (red(0.0), red(800.0));
    assert!(
        lit > dark + 40,
        "the sphere should catch the small red light: max r {lit} lit, {dark} unlit"
    );
}
