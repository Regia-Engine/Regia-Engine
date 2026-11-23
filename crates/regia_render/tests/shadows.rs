// SPDX-License-Identifier: MPL-2.0
//! Sun cascades plus point/spot atlas shadows.

use regia_render::{
    DirectionalLightUniform, LabLocalLight, LabMaterial, LabMesh, LabPointLight, LabScene, LabSpotLight,
    LabVertex, RenderDevice, render_lab_scene,
};

fn lit_floor_scene(shadows: bool) -> LabScene {
    let h = 2.5;
    let corners = [[-h, 0.0, -h], [h, 0.0, -h], [h, 0.0, h], [-h, 0.0, h]];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let vertices = corners
        .into_iter()
        .zip(uvs)
        .map(|(position, uv)| LabVertex {
            position,
            normal: [0.0, 1.0, 0.0],
            uv,
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0, 1.0, 1.0, 1.0],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect();
    let (sphere, sphere_idx) = regia_render::uv_sphere(0.45, 16, 12);
    let sphere_verts = sphere
        .iter()
        .map(|v| {
            let mut lab = *v;
            lab.position[1] += 0.45;
            lab
        })
        .collect();
    LabScene {
        width: 160,
        height: 120,
        eye: [1.6, 2.2, 3.2],
        target: [0.0, 0.3, 0.0],
        fov_y: 45.0_f32.to_radians(),
        near: 0.1,
        far: 30.0,
        depth_prepass: true,
        directional_shadows: shadows,
        ibl: false,
        exposure_ev: 14.8,
        ambient_lux: 10_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.35, -1.0, 0.25], [1.0, 0.98, 0.94], 100_000.0),
        local_lights: vec![
            LabLocalLight::Point(LabPointLight {
                position: [-1.2, 1.8, 0.4],
                color: [1.0, 0.3, 0.2],
                intensity: 350.0,
                radius: 5.0,
                cast_shadows: shadows,
                source_radius: None,
            }),
            LabLocalLight::Spot(LabSpotLight {
                position: [1.4, 2.0, 1.2],
                direction: [-0.45, -0.8, -0.35],
                color: [0.3, 0.5, 1.0],
                intensity: 60.0,
                range: 6.0,
                inner_angle: 0.2,
                outer_angle: 0.55,
                cast_shadows: shadows,
                source_radius: None,
            }),
        ],
        materials: vec![LabMaterial {
            base_color: [0.75, 0.75, 0.78, 1.0],
            roughness: 0.6,
            ..LabMaterial::default()
        }],
        meshes: vec![
            LabMesh {
                vertices,
                indices: vec![0, 2, 1, 0, 3, 2],
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: sphere_verts,
                indices: sphere_idx,
                material: 0,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn mean_luma(pixels: &[u8]) -> f32 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for px in pixels.as_chunks::<4>().0 {
        sum += px[0] as f32 + px[1] as f32 + px[2] as f32;
        n += 1.0;
    }
    sum / n
}

#[test]
fn shadows_darken_the_receiver() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip shadows: {err}");
            return;
        }
    };
    let lit = render_lab_scene(&device, &lit_floor_scene(false)).expect("lit");
    let shaded = render_lab_scene(&device, &lit_floor_scene(true)).expect("shaded");
    let lit_l = mean_luma(&lit.pixels);
    let shaded_l = mean_luma(&shaded.pixels);
    assert!(
        shaded_l + 1.0 < lit_l,
        "shadowed frame should be darker (lit {lit_l}, shaded {shaded_l})"
    );
}
