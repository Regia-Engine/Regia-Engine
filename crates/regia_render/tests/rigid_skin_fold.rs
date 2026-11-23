// SPDX-License-Identifier: MPL-2.0
//! One-joint rigid skins fold into the instance transform (DO27 PERF_JANK item 7).
//!
//! A part skinned to a single joint (a canopy, a fan, a door) used to draw one by one with a CPU
//! uniform, uncull'd and outside the visibility buffer. With its joint folded into the model
//! matrix it batches into the GPU-driven instanced lists and renders the same picture.

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_render::{
    DirectionalLightUniform, DrawStats, FrameQuality, FrameRenderer, JointMatrix, LabMaterial, LabMesh,
    LabScene, LabVertex, PostSettings, RenderDevice,
};

const W: u32 = 128;
const H: u32 = 96;
const PARTS: usize = 16;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn gpu_driven(device: &RenderDevice) -> bool {
    device.capabilities.multi_draw_indirect && device.capabilities.indirect_first_instance
}

/// A unit box around the origin, every vertex skinned to joint 0 with weight 1 (`weights`
/// overrides that for the negative tests).
fn part(weights: [f32; 4], joints: [u32; 4]) -> Vec<LabVertex> {
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
    ];
    let mut vertices = Vec::new();
    for (n, u, v) in faces {
        for (a, b) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [
                0.25 * (n[0] + a * u[0] + b * v[0]),
                0.25 * (n[1] + a * u[1] + b * v[1]),
                0.25 * (n[2] + a * u[2] + b * v[2]),
            ];
            let mut vertex = LabVertex::from_pos_normal_uv(p, n, [0.5 + 0.5 * a, 0.5 - 0.5 * b]);
            vertex.weights = weights;
            vertex.joints = joints;
            vertices.push(vertex);
        }
    }
    vertices
}

fn box_indices() -> Vec<u32> {
    (0..6u32)
        .flat_map(|face| [0, 1, 2, 0, 2, 3].map(|i| face * 4 + i))
        .collect()
}

/// The joint of part `i`: a turn about Y and a lift (a door on its hinge).
fn joint(i: usize) -> Mat4 {
    Mat4::from_rotation_translation(
        Quat::from_rotation_y(0.35 + i as f32 * 0.4),
        Vec3::new(0.05 * i as f32, 0.3, -0.1),
    )
}

fn model(i: usize) -> Mat4 {
    let (x, z) = ((i % 4) as f32 * 1.1 - 1.65, (i / 4) as f32 * 1.1 - 1.65);
    Mat4::from_translation(Vec3::new(x, 0.0, z))
}

/// Ground plus a 4 × 4 field of boxes. `skinned`: each box carries its joint as a one-joint
/// palette; otherwise the joint is baked into its model matrix (the rigid reference).
fn field(skinned: bool, weights: [f32; 4], joints: [u32; 4]) -> LabScene {
    let ground = [
        [-4.0, 0.0, -4.0],
        [4.0, 0.0, -4.0],
        [4.0, 0.0, 4.0],
        [-4.0, 0.0, 4.0],
    ];
    let mut meshes = vec![LabMesh {
        vertices: ground
            .iter()
            .map(|&p| LabVertex::from_pos_normal_uv(p, [0.0, 1.0, 0.0], [0.0, 0.0]))
            .collect(),
        indices: vec![0, 2, 1, 0, 3, 2],
        ..LabMesh::unskinned()
    }];
    for i in 0..PARTS {
        let (model, palette) = if skinned {
            (model(i), vec![JointMatrix::from_mat4(joint(i))])
        } else {
            (model(i) * joint(i), Vec::new())
        };
        meshes.push(LabMesh {
            vertices: part(if skinned { weights } else { [0.0; 4] }, joints),
            indices: box_indices(),
            material: 1,
            model: model.to_cols_array_2d(),
            joints: palette,
        });
    }
    LabScene {
        width: W,
        height: H,
        eye: [3.5, 3.0, 4.5],
        target: [0.0, 0.2, 0.0],
        fov_y: 0.8,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 2_000.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([-0.4, -0.8, -0.3], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                roughness: 0.9,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.35, 0.2, 1.0],
                roughness: 0.5,
                ..LabMaterial::default()
            },
        ],
        meshes,
    }
}

fn render(device: &RenderDevice, scene: &LabScene, gpu: bool, fold: bool) -> (Vec<u8>, DrawStats) {
    let frame = scene.as_frame_scene_with_quality(FrameQuality {
        gpu_driven: gpu,
        ..FrameQuality::LAB
    });
    let mut renderer = FrameRenderer::new();
    renderer.set_rigid_skin_fold(fold);
    // The unfolded arm is the skinned per-object path the fold replaces. Skinned instancing
    // (ADR-1368, on by default) would batch the unfolded parts too; that arm has its own test.
    renderer.set_skinned_instancing(false);
    let image = renderer.render_offscreen(device, &frame).expect("frame");
    (image.pixels, renderer.draw_stats())
}

/// Max channel difference and the pixels differing by more than `tolerance`.
fn compare(a: &[u8], b: &[u8], tolerance: u8) -> (u8, usize) {
    let mut max = 0u8;
    let mut over = 0usize;
    for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let d = pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        max = max.max(d);
        if d > tolerance {
            over += 1;
        }
    }
    (max, over)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn one_joint_rigid_skins_draw_instanced_with_the_same_pixels() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let skinned = field(true, [1.0, 0.0, 0.0, 0.0], [0; 4]);
    let rigid = field(false, [0.0; 4], [0; 4]);
    let (folded_px, folded) = render(&device, &skinned, true, true);
    let (unfolded_px, unfolded) = render(&device, &skinned, true, false);
    let (rigid_px, reference) = render(&device, &rigid, true, true);
    eprintln!("folded {folded:?}\nunfolded {unfolded:?}\nrigid {reference:?}");
    assert_eq!(folded.rigid_skin_folded, PARTS, "{folded:?}");
    assert_eq!(unfolded.rigid_skin_folded, 0, "{unfolded:?}");
    if gpu_driven(&device) {
        // Before: every part one by one on the CPU loop. After: none.
        assert_eq!(unfolded.per_object, PARTS, "{unfolded:?}");
        assert_eq!(folded.per_object, reference.per_object, "{folded:?}");
        assert_eq!(folded.per_object, 0, "{folded:?}");
        assert_eq!(folded.instanced, PARTS + 1, "{folded:?}");
    } else {
        eprintln!("device has no indirect first-instance: no instanced path to compare");
    }
    // The folded parts are the rigid reference exactly (same matrices, same path).
    let (max, over) = compare(&folded_px, &rigid_px, 0);
    eprintln!("folded vs rigid reference: max {max}/255, {over} px differ");
    assert_eq!(
        over, 0,
        "a folded one-joint skin must render its rigid equivalent (max {max})"
    );
    // And the skinned per-object frame up to float rounding of `model × (joint × p)` against
    // `(model × joint) × p`.
    let (max, over) = compare(&folded_px, &unfolded_px, 1);
    eprintln!("folded vs skinned per-object: max {max}/255, {over} px over 1/255");
    assert_eq!(
        over, 0,
        "the fold must not change the picture: {over} px over 1/255 (max {max})"
    );
    assert!(folded_px.iter().any(|byte| *byte > 40), "the parts are lit");
}

#[test]
fn the_per_object_path_folds_too() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let skinned = field(true, [1.0, 0.0, 0.0, 0.0], [0; 4]);
    let rigid = field(false, [0.0; 4], [0; 4]);
    let (folded_px, folded) = render(&device, &skinned, false, true);
    let (rigid_px, _) = render(&device, &rigid, false, true);
    assert_eq!(folded.rigid_skin_folded, PARTS, "{folded:?}");
    let (max, over) = compare(&folded_px, &rigid_px, 0);
    assert_eq!(over, 0, "per-object folded vs rigid: max {max}");
}

#[test]
fn blended_or_unweighted_skins_keep_the_skinned_path() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    // Weights that do not sum to one, a weighted lane on another joint, and no weights at all
    // (unskinned vertices in a skinned mesh): the fold would move them, so none folds.
    for (weights, joints) in [
        ([0.5, 0.0, 0.0, 0.0], [0; 4]),
        ([0.5, 0.5, 0.0, 0.0], [0, 1, 0, 0]),
        ([0.0; 4], [0; 4]),
    ] {
        let (_, stats) = render(&device, &field(true, weights, joints), true, true);
        assert_eq!(stats.rigid_skin_folded, 0, "{weights:?} {joints:?}: {stats:?}");
    }
    // Split weights on joint 0 alone still sum to one: they fold.
    let (_, stats) = render(&device, &field(true, [0.25, 0.75, 0.0, 0.0], [0; 4]), true, true);
    assert_eq!(stats.rigid_skin_folded, PARTS, "{stats:?}");
}

#[test]
fn many_one_joint_parts_leave_the_cpu_loop() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    if !gpu_driven(&device) {
        eprintln!("device has no indirect first-instance");
        return;
    }
    let skinned = field(true, [1.0, 0.0, 0.0, 0.0], [0; 4]);
    let frame = skinned.as_frame_scene_with_quality(FrameQuality {
        gpu_driven: true,
        ..FrameQuality::LAB
    });
    let mut renderer = FrameRenderer::new();
    // Measured against the skinned per-object path (skinned instancing off, ADR-1368).
    renderer.set_skinned_instancing(false);
    for fold in [false, true] {
        renderer.set_rigid_skin_fold(fold);
        renderer.render_offscreen(&device, &frame).expect("frame");
        let stats = renderer.draw_stats();
        eprintln!(
            "fold {fold}: per_object {} instanced {}",
            stats.per_object, stats.instanced
        );
        let expected = if fold { 0 } else { PARTS };
        assert_eq!(stats.per_object, expected, "fold {fold}: {stats:?}");
    }
}

/// With skinned instancing on (ADR-1368, the default) the unfolded one-joint parts leave the CPU
/// loop through the skinned instanced batches instead, and still render the folded picture up
/// to float rounding.
#[test]
fn unfolded_one_joint_skins_batch_through_skinned_instancing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    if !gpu_driven(&device) {
        eprintln!("device has no indirect first-instance");
        return;
    }
    let skinned = field(true, [1.0, 0.0, 0.0, 0.0], [0; 4]);
    let frame = skinned.as_frame_scene_with_quality(FrameQuality {
        gpu_driven: true,
        ..FrameQuality::LAB
    });
    let mut renderer = FrameRenderer::new();
    assert!(
        renderer.skinned_instancing(),
        "skinned instancing is on by default"
    );
    renderer.set_rigid_skin_fold(false);
    let unfolded_px = renderer.render_offscreen(&device, &frame).expect("frame").pixels;
    let unfolded = renderer.draw_stats();
    eprintln!("unfolded, skinned instancing {unfolded:?}");
    assert_eq!(unfolded.rigid_skin_folded, 0, "{unfolded:?}");
    assert_eq!(unfolded.per_object, 0, "{unfolded:?}");
    assert_eq!(unfolded.skinned_draws, PARTS, "{unfolded:?}");
    assert_eq!(unfolded.skinned_instanced, PARTS, "{unfolded:?}");
    let (folded_px, folded) = render(&device, &skinned, true, true);
    assert_eq!(folded.rigid_skin_folded, PARTS, "{folded:?}");
    let (max, over) = compare(&folded_px, &unfolded_px, 1);
    eprintln!("folded vs skinned instanced: max {max}/255, {over} px over 1/255");
    assert_eq!(
        over, 0,
        "skinned instancing must not change the picture: {over} px over 1/255 (max {max})"
    );
}
