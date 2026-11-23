// SPDX-License-Identifier: MPL-2.0
//! Skinned meshes are frustum-culled by a conservative bound from their joint palette (ADR-248,
//! `frame/skin_bounds.rs`), never by the bind pose.
//!
//! - A joint that moves the vertices far from the bind pose (bind pose off-screen, posed mesh in
//!   view; also a 50/50 blend of two joints whose own boxes are both off-screen) keeps the mesh
//!   drawn, pixel-identical to the bounds switched off.
//! - A skinned mesh posed behind the camera, or bound there, is culled, and the frame is
//!   pixel-identical to the bounds switched off.
//! - A culled skinned mesh still casts its shadow into the view.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, FrameScene, JointMatrix, LabMaterial, LabMesh,
    LabScene, LabVertex, RenderDevice,
};

const W: u32 = 64;
const H: u32 = 48;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A quad facing +Z centred on `(x, y, z)`, every vertex weighted by `weights` to joints 0 and 1.
fn quad(center: [f32; 3], size: f32, weights: [f32; 2]) -> LabMesh {
    let h = size * 0.5;
    let [x, y, z] = center;
    let mut vertices = vec![
        LabVertex::from_pos_normal_uv([x - h, y - h, z], [0.0, 0.0, 1.0], [0.0, 1.0]),
        LabVertex::from_pos_normal_uv([x + h, y - h, z], [0.0, 0.0, 1.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([x + h, y + h, z], [0.0, 0.0, 1.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([x - h, y + h, z], [0.0, 0.0, 1.0], [0.0, 0.0]),
    ];
    for v in &mut vertices {
        v.joints = [0, 1, 0, 0];
        v.weights = [weights[0], weights[1], 0.0, 0.0];
    }
    LabMesh {
        vertices,
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

/// A horizontal quad facing up at height `y` (a caster above the view, or the ground).
fn flat(y: f32, half: f32, skinned: bool) -> LabMesh {
    let mut vertices = vec![
        LabVertex::from_pos_normal_uv([-half, y, -half], [0.0, 1.0, 0.0], [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([-half, y, half], [0.0, 1.0, 0.0], [0.0, 1.0]),
        LabVertex::from_pos_normal_uv([half, y, half], [0.0, 1.0, 0.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([half, y, -half], [0.0, 1.0, 0.0], [1.0, 0.0]),
    ];
    let mut joints = Vec::new();
    if skinned {
        for v in &mut vertices {
            v.weights = [1.0, 0.0, 0.0, 0.0];
        }
        joints = vec![JointMatrix::identity(), JointMatrix::identity()];
    }
    LabMesh {
        vertices,
        indices: vec![0, 1, 2, 0, 2, 3],
        joints,
        ..LabMesh::unskinned()
    }
}

fn lab(meshes: Vec<LabMesh>, sun: [f32; 3]) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 4.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 40.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 3_000.0,
        double_sided: true,
        post: None,
        directional: DirectionalLightUniform::new(sun, [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.8, 0.7, 0.6, 1.0],
            ..LabMaterial::default()
        }],
        meshes,
    }
}

fn keyed(lab: &LabScene, quality: FrameQuality) -> FrameScene<'_> {
    let mut scene = lab.as_frame_scene_with_quality(quality);
    for (i, mesh) in scene.meshes.iter_mut().enumerate() {
        mesh.mesh_key = 0x5B_0000 + i as u64;
        mesh.object_key = 0x5B_1000 + i as u64;
        mesh.material_key = 0x5B_2000;
    }
    scene
}

fn qualities() -> [FrameQuality; 2] {
    [
        FrameQuality::default(),
        FrameQuality {
            gpu_driven: true,
            ..FrameQuality::default()
        },
    ]
}

/// The frame with skinned bounds on and off, the last frame's counts with them on.
fn on_and_off(
    device: &RenderDevice,
    scene: &FrameScene<'_>,
) -> (
    Vec<u8>,
    Vec<u8>,
    regia_render::DrawIntegrity,
    regia_render::DrawStats,
) {
    let mut on = FrameRenderer::new();
    let image_on = on.render_offscreen(device, scene).expect("bounds on").pixels;
    let mut off = FrameRenderer::new();
    off.set_skinned_bounds(false);
    let image_off = off.render_offscreen(device, scene).expect("bounds off").pixels;
    assert_eq!(off.draw_integrity().skinned_culled, 0);
    (image_on, image_off, on.draw_integrity(), on.draw_stats())
}

fn lit_pixels(image: &[u8], background: &[u8]) -> usize {
    image
        .as_chunks::<4>()
        .0
        .iter()
        .zip(background.as_chunks::<4>().0)
        .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 4))
        .count()
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_joint_moved_far_from_the_bind_pose_keeps_the_mesh_drawn() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let sun = [0.3, -0.5, -1.0];
    let empty = lab(Vec::new(), sun);
    // Bind pose 30 m to the right (off-screen); joint 1 brings it back to the centre.
    let mut moved = quad([30.0, 0.0, 0.0], 0.8, [0.0, 1.0]);
    moved.joints = vec![JointMatrix::identity(), JointMatrix::translation(-30.0, 0.0, 0.0)];
    // Bind pose 30 m to the right; a 50/50 blend of identity and a 60 m move lands it at the
    // centre while each joint's own box is off-screen (left and right).
    let mut blended = quad([30.0, 0.0, 0.0], 0.8, [0.5, 0.5]);
    blended.joints = vec![JointMatrix::identity(), JointMatrix::translation(-60.0, 0.0, 0.0)];
    for quality in qualities() {
        let background = FrameRenderer::new()
            .render_offscreen(&device, &keyed(&empty, quality))
            .expect("empty")
            .pixels;
        for (name, mesh) in [("moved", &moved), ("blended", &blended)] {
            let scene = lab(vec![mesh.clone()], sun);
            let (image_on, image_off, integrity, stats) = on_and_off(&device, &keyed(&scene, quality));
            assert_eq!(integrity.skinned_bounded, 1, "{name}: {integrity:?}");
            assert_eq!(integrity.skinned_culled, 0, "{name}: {integrity:?}");
            assert_eq!(stats.drawn, 1, "{name}: {stats:?}");
            assert!(
                lit_pixels(&image_on, &background) > 50,
                "{name}: the posed quad is on screen"
            );
            assert_eq!(image_on, image_off, "{name}, gpu_driven {}", quality.gpu_driven);
        }
    }
}

#[test]
fn a_skinned_mesh_behind_the_camera_is_culled() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let sun = [0.3, -0.5, -1.0];
    // Bound in view, posed 10 m behind the eye (the camera looks down -Z from z = 4).
    let mut posed = quad([0.0, 0.0, 0.0], 0.8, [0.0, 1.0]);
    posed.joints = vec![JointMatrix::identity(), JointMatrix::translation(0.0, 0.0, 14.0)];
    // Bound behind the eye, identity pose.
    let mut bound = quad([0.0, 0.0, 12.0], 0.8, [1.0, 0.0]);
    bound.joints = vec![JointMatrix::identity(), JointMatrix::identity()];
    for quality in qualities() {
        for (name, mesh) in [("posed", &posed), ("bound", &bound)] {
            let scene = lab(vec![mesh.clone()], sun);
            let (image_on, image_off, integrity, stats) = on_and_off(&device, &keyed(&scene, quality));
            assert_eq!(integrity.skinned_bounded, 1, "{name}: {integrity:?}");
            assert_eq!(integrity.skinned_culled, 1, "{name}: {integrity:?}");
            assert_eq!(stats.drawn, 0, "{name}: {stats:?}");
            assert_eq!(stats.culled, 1, "{name}: {stats:?}");
            assert_eq!(image_on, image_off, "{name}, gpu_driven {}", quality.gpu_driven);
        }
    }
}

#[test]
fn a_culled_skinned_mesh_still_casts_its_shadow() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    // Sun nearly straight down: the caster 4 m up is above the view, its shadow on the ground
    // in it.
    let sun = [0.0, -1.0, -0.05];
    let ground = flat(-1.0, 6.0, false);
    let caster = flat(4.0, 1.2, true);
    let without = lab(vec![ground.clone()], sun);
    let with = lab(vec![ground, caster], sun);
    for quality in qualities() {
        let no_caster = FrameRenderer::new()
            .render_offscreen(&device, &keyed(&without, quality))
            .expect("ground only")
            .pixels;
        let (image_on, image_off, integrity, _) = on_and_off(&device, &keyed(&with, quality));
        assert_eq!(integrity.skinned_culled, 1, "{integrity:?}");
        assert!(
            lit_pixels(&image_on, &no_caster) > 20,
            "the shadow reaches the ground in view"
        );
        assert_eq!(image_on, image_off, "gpu_driven {}", quality.gpu_driven);
    }
}
