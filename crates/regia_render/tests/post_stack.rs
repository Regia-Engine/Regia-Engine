// SPDX-License-Identifier: MPL-2.0
//! Post-stack switches on the lab frame.

use regia_render::{
    DirectionalLightUniform, LabMaterial, LabMesh, LabScene, PostSettings, RenderDevice, TonemapOperator,
    render_lab_scene, uv_sphere,
};

fn emissive_sphere(post: Option<PostSettings>) -> LabScene {
    let (verts, indices) = uv_sphere(0.4, 12, 8);
    let vertices: Vec<_> = verts.to_vec();
    LabScene {
        width: 64,
        height: 64,
        eye: [0.0, 0.2, 1.8],
        target: [0.0, 0.0, 0.0],
        fov_y: 45.0_f32.to_radians(),
        near: 0.1,
        far: 10.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: false,
        post,
        directional: DirectionalLightUniform::new([0.0, -0.2, -1.0], [1.0, 1.0, 1.0], 0.2),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.02, 0.02, 0.02, 1.0],
            emissive: [1.0, 0.8, 0.3],
            emissive_intensity: 6.0,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices,
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

#[test]
fn bloom_changes_the_frame() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip post stack: {err}");
            return;
        }
    };
    let off = render_lab_scene(&device, &emissive_sphere(None)).expect("off");
    let on = PostSettings {
        enabled: true,
        bloom: true,
        bloom_mips: 3,
        auto_exposure: false,
        exposure_ev: 0.0,
        tonemap: TonemapOperator::AgX,
        fxaa: false,
        taa: false,
        vignette: 0.0,
        film_grain: 0.0,
        ..PostSettings::default()
    };
    let bloom = render_lab_scene(&device, &emissive_sphere(Some(on))).expect("bloom");
    assert_ne!(off.pixels, bloom.pixels);
}
