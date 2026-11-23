// SPDX-License-Identifier: MPL-2.0
//! GPU skinning from a stored joint palette (`specs/13` § 8).

use regia_render::{
    DirectionalLightUniform, JointMatrix, LabMaterial, LabMesh, LabScene, LabVertex, RenderDevice,
    render_lab_scene,
};

fn bar(weights: [f32; 4], joints: Vec<JointMatrix>) -> LabScene {
    let positions = [
        [-0.08, -0.55, 0.0],
        [0.08, -0.55, 0.0],
        [0.08, 0.55, 0.0],
        [-0.08, 0.55, 0.0],
    ];
    let vertices = positions
        .into_iter()
        .map(|position| LabVertex {
            position,
            normal: [0.0, 0.0, 1.0],
            uv: [0.0, 0.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights,
        })
        .collect();
    LabScene {
        width: 64,
        height: 64,
        eye: [0.0, 0.0, 2.2],
        target: [0.0, 0.0, 0.0],
        fov_y: 45.0_f32.to_radians(),
        near: 0.1,
        far: 10.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.8,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.3, -1.0], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.85, 0.2, 0.15, 1.0],
            roughness: 0.5,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices,
            indices: vec![0, 1, 2, 0, 2, 3],
            material: 0,
            joints,
            ..LabMesh::unskinned()
        }],
    }
}

#[test]
fn stored_joint_moves_the_mesh() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip skinning: {err}");
            return;
        }
    };
    let rigid = render_lab_scene(&device, &bar([0.0; 4], Vec::new())).expect("rigid");
    let translated = JointMatrix::translation(0.55, 0.0, 0.0);
    let skinned = render_lab_scene(&device, &bar([1.0, 0.0, 0.0, 0.0], vec![translated])).expect("skinned");
    assert_ne!(rigid.pixels, skinned.pixels);
    assert!(
        rigid.pixels.iter().any(|byte| *byte > 20),
        "rigid bar should be visible"
    );
    assert!(
        skinned.pixels.iter().any(|byte| *byte > 20),
        "skinned bar should be visible"
    );
}
