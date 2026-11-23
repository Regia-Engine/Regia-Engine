// SPDX-License-Identifier: MPL-2.0
//! Object keys and motion-vector pairing (v0.1.15 follow-up).
//!
//! The renderer pairs a mesh with last frame's model matrix by `object_key`. Key `0` is
//! ephemeral: the mesh is keyed by its index in the frame's mesh list instead, so a spawn, a
//! despawn or a reorder pairs it with another object's matrix (known behavior, documented in
//! the host checklist's "Object keys"). `DrawStats::zero_key_moving_meshes` counts zero-key
//! meshes whose matrix changed at their list index; real keys never count.

use regia_render::{
    DirectionalLightUniform, DrawStats, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, ZERO_KEY_WARN_MIN,
};

const COUNT: usize = 6;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn translation(x: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, 0.5, 0.0, 1.0],
    ]
}

fn scene() -> LabScene {
    let meshes = (0..COUNT)
        .map(|i| {
            let (vertices, indices) = regia_render::box_mesh([0.2, 0.2, 0.2]);
            LabMesh {
                vertices,
                indices,
                material: 0,
                model: translation(i as f32 * 0.6 - 1.5),
                ..LabMesh::unskinned()
            }
        })
        .collect();
    LabScene {
        width: 96,
        height: 64,
        eye: [0.0, 1.5, 6.0],
        target: [0.0, 0.5, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0, 1.0, 1.0], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes,
    }
}

fn quality() -> FrameQuality {
    let mut q = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    q.screen_space_gi = false;
    q.ssr = false;
    q.ssao = regia_render::SsaoMode::Off;
    q.reflection_probes = false;
    q.volumetric_fog = false;
    q.volumetric_clouds = false;
    q.physical_atmosphere = false;
    q.taau = true;
    q
}

/// Renders the frames of one run: `zero` submits every mesh with `object_key 0`, otherwise
/// the lab's real keys stay. Frame 0 is the scene as built, frame 1 the same objects in
/// reversed list order, frame 2 the reversed order again (nothing changed).
fn run(device: &RenderDevice, zero: bool) -> Vec<DrawStats> {
    let lab = scene();
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        taa: false,
        ..PostSettings::golden_deterministic()
    };
    let mut frame = lab.as_frame_scene_with_quality(quality());
    frame.post = Some(&post);
    if zero {
        for mesh in &mut frame.meshes {
            mesh.object_key = 0;
        }
    }
    let mut renderer = FrameRenderer::new();
    let mut stats = Vec::new();
    for step in 0..3 {
        if step == 1 {
            frame.meshes.reverse();
        }
        renderer.render_offscreen(device, &frame).expect("frame");
        stats.push(renderer.draw_stats());
    }
    stats
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn zero_keys_pair_by_list_index() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let stats = run(&device, true);
    // The first frame has no history; nothing counts.
    assert_eq!(stats[0].zero_key_moving_meshes, 0, "{:?}", stats[0]);
    // Reversing the list moves every mesh to another index: all COUNT matrices differ from
    // the ones at their index (the middle pair of a reversal of 6 also swaps), so the whole
    // list reads as moving. That is the known zero-key behavior: pairing is by index.
    assert_eq!(stats[1].zero_key_moving_meshes, COUNT, "{:?}", stats[1]);
    assert!(stats[1].zero_key_moving_meshes >= ZERO_KEY_WARN_MIN);
    // Same order again: static, no warning material.
    assert_eq!(stats[2].zero_key_moving_meshes, 0, "{:?}", stats[2]);
}

#[test]
fn real_keys_pair_by_identity() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let stats = run(&device, false);
    for (i, s) in stats.iter().enumerate() {
        assert_eq!(s.zero_key_moving_meshes, 0, "frame {i}: {s:?}");
    }
}
