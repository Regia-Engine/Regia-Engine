// SPDX-License-Identifier: MPL-2.0
//! Warm coverage of compute skinning (Tincal B2): after `FrameRenderer::prewarm` and one
//! vertex-path frame, switching skinned meshes to the compute path (linear, dual quaternion,
//! with motion vectors) builds no pipeline: the skinning kernel is built by the warm-up and the
//! pre-skinned velocity pipelines with the other velocity pipelines.
//!
//! Its own test binary: the pipeline counter is process-wide.

use regia_core::glam::Mat4;
use regia_render::{
    ALPHA_OPAQUE, DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
    JointMatrix, LabMaterial, LabVertex, PostSettings, RenderDevice, ShadowCast, SkinningOptions,
};

fn limbs() -> (Vec<LabVertex>, Vec<u32>) {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for (x0, x1, joint) in [(-1.4f32, -0.4f32, 0u32), (0.4, 1.4, 1)] {
        let base = vertices.len() as u32;
        for p in [[x0, -0.3, 0.0], [x1, -0.3, 0.0], [x1, 0.3, 0.0], [x0, 0.3, 0.0]] {
            let mut v = LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0, 0.0]);
            v.joints = [joint, 0, 0, 0];
            v.weights = [1.0, 0.0, 0.0, 0.0];
            vertices.push(v);
        }
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

#[test]
fn compute_skinning_after_warm_up_builds_no_pipeline() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let geometry = limbs();
    let material = LabMaterial {
        base_color: [0.8, 0.8, 0.8, 1.0],
        alpha_mode: ALPHA_OPAQUE,
        ..LabMaterial::default()
    };
    let post = PostSettings {
        exposure_ev: 14.5,
        ..PostSettings::golden_deterministic()
    };
    fn scene<'a>(
        joints: &'a [JointMatrix],
        geometry: &'a (Vec<LabVertex>, Vec<u32>),
        material: &'a LabMaterial,
        post: &'a PostSettings,
    ) -> FrameScene<'a> {
        let mut quality = FrameQuality::LAB;
        quality.taau = true;
        FrameScene {
            width: 96,
            height: 64,
            eye: [0.0, 0.0, 3.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.05,
            far: 50.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: false,
            exposure_ev: 14.5,
            ambient_lux: 200.0,
            double_sided: false,
            post: Some(post),
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([0.3, -1.0, -0.2], [1.0, 1.0, 1.0], 50_000.0),
            local_lights: &[],
            quality,
            meshes: vec![FrameMesh {
                is_static: false,
                mesh_key: 0x51C1,
                material_key: 1,
                object_key: 21,
                vertices: &geometry.0,
                indices: &geometry.1,
                material,
                model: Mat4::IDENTITY.to_cols_array_2d(),
                joints,
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }],
        }
    }
    let pose = |dx: f32| [JointMatrix::identity(), JointMatrix::translation(dx, 0.0, 0.0)];
    let mut renderer = FrameRenderer::new();
    let first = pose(0.0);
    renderer
        .prewarm(&device, &scene(&first, &geometry, &material, &post).meshes, 16)
        .expect("prewarm");
    for dx in [0.0, 0.1] {
        let joints = pose(dx);
        renderer
            .render_offscreen(&device, &scene(&joints, &geometry, &material, &post))
            .expect("vertex-path frame");
    }
    let created = regia_render::pipeline_cache::pipelines_created();
    renderer.set_compute_skinning(true);
    for dx in [0.2, 0.3, 0.3, 0.3] {
        let joints = pose(dx);
        renderer
            .render_offscreen(&device, &scene(&joints, &geometry, &material, &post))
            .expect("compute frame");
    }
    assert_eq!(renderer.skinning_stats().instances, 1);
    renderer.set_skinning_options([(21, SkinningOptions::dual_quaternion())]);
    for dx in [0.4, 0.5] {
        let joints = pose(dx);
        renderer
            .render_offscreen(&device, &scene(&joints, &geometry, &material, &post))
            .expect("dual quaternion frame");
    }
    assert_eq!(renderer.skinning_stats().dual_quaternion, 1);
    assert_eq!(
        regia_render::pipeline_cache::pipelines_created(),
        created,
        "a compute skinning frame after warm-up created a pipeline"
    );
}
