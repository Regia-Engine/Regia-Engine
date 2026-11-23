// SPDX-License-Identifier: MPL-2.0
//! Review w11 #11: a committed edit of `skin_compute.wgsl` (dev builds' engine-shader hot reload,
//! `docs/specs/12-assets.md` § 8) reaches the compute skinning kernel. The kernel used to be
//! cached under a key without the shader epoch and carried across the renderer's hot swap, so the
//! edit was not picked up until a restart.
//!
//! This binary holds one test: the shader override map is process-wide.

use regia_render::shader_reload::{clear_shader_overrides, shader_epoch, stage_shader_edit};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, JointMatrix,
    LabMaterial, LabVertex, RenderDevice, SkinningOptions, skin_on_gpu,
};

const KERNEL: &str = include_str!("../shaders/skin_compute.wgsl");
const STORE: &str = "    store3(base, p);";

/// A 4 m quad on the ground, every vertex bound to joint 0 with full weight.
fn quad() -> Vec<LabVertex> {
    [[-2.0f32, -2.0], [2.0, -2.0], [2.0, 2.0], [-2.0, 2.0]]
        .into_iter()
        .map(|[x, z]| LabVertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [1.0, 0.0, 0.0, 0.0],
        })
        .collect()
}

/// Brightness of the centre pixel, seen from above with the quad filling the view.
fn render(renderer: &mut FrameRenderer, device: &RenderDevice, vertices: &[LabVertex]) -> f32 {
    let indices = [0u32, 2, 1, 0, 3, 2];
    let joints = [JointMatrix::identity()];
    let material = LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        ..LabMaterial::default()
    };
    let scene = FrameScene {
        width: 32,
        height: 32,
        eye: [0.0, 4.0, 0.01],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.1,
        far: 50.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 0.0,
        ambient_lux: 0.0,
        double_sided: true,
        post: None,
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0, 1.0, 1.0], 0.8),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes: vec![FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1,
            object_key: 1,
            vertices,
            indices: &indices,
            material: &material,
            model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
            joints: &joints,
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: regia_render::LOD_DITHER_NONE,
        }],
    };
    let pixels = renderer.render_offscreen(device, &scene).expect("frame").pixels;
    let i = ((16 * 32 + 16) * 4) as usize;
    (f32::from(pixels[i]) + f32::from(pixels[i + 1]) + f32::from(pixels[i + 2])) / 3.0
}

#[test]
fn an_edited_skin_kernel_is_picked_up_without_a_restart() {
    let device = RenderDevice::headless().unwrap_or_else(|e| panic!("no headless GPU (lavapipe?): {e}"));
    clear_shader_overrides();
    let vertices = quad();
    let joints = [JointMatrix::identity()];

    let mut renderer = FrameRenderer::new();
    renderer.set_compute_skinning(true);
    // A one-joint skin bound rigidly to its joint folds into a rigid draw by default
    // (`set_rigid_skin_fold`) and never reaches the kernel: the gate measured 0 compute
    // instances here. Keep the quad on the skinned path so the kernel is what draws it.
    renderer.set_rigid_skin_fold(false);
    let before = render(&mut renderer, &device, &vertices);
    assert!(before > 30.0, "the skinned quad is lit: {before}");
    assert!(
        renderer.skinning_stats().instances > 0,
        "the quad took the compute path: {:?}",
        renderer.skinning_stats()
    );
    // Held still for a few frames the twin settles: no dispatch, both buffers hold the pose.
    for _ in 0..3 {
        render(&mut renderer, &device, &vertices);
    }
    assert_eq!(renderer.skinning_stats().dispatched, 0, "the twin settled");
    let kernel_before =
        skin_on_gpu(&device, &vertices, &joints, &SkinningOptions::compute()).expect("kernel");
    assert_eq!(kernel_before[0].position, vertices[0].position);

    // The edit moves every skinned vertex 100 m away: the quad leaves the view.
    assert_eq!(
        KERNEL.matches(STORE).count(),
        1,
        "the kernel's position store moved"
    );
    let edited = KERNEL.replacen(STORE, "    store3(base, p + vec3<f32>(100.0, 0.0, 0.0));", 1);
    let epoch = shader_epoch();
    stage_shader_edit("skin_compute.wgsl", &edited).expect("a valid kernel edit");
    assert_eq!(shader_epoch(), epoch + 1);

    let kernel_after = skin_on_gpu(&device, &vertices, &joints, &SkinningOptions::compute()).expect("kernel");
    let after = render(&mut renderer, &device, &vertices);
    clear_shader_overrides();
    assert!(
        (kernel_after[0].position[0] - (vertices[0].position[0] + 100.0)).abs() < 1e-3,
        "a fresh kernel build uses the edited source: {:?}",
        kernel_after[0].position
    );
    assert!(
        after < before * 0.5,
        "the renderer's hot swap rebuilt the kernel: centre {after} (was {before})"
    );
}
