// SPDX-License-Identifier: MPL-2.0
//! Memory pressure trims the idle mesh and material caches (`signal_memory_pressure`).
//!
//! Under the budget idle entries stay resident ([`CacheResidency`]); an OS low-memory event or
//! the VRAM probe must still give that memory back. Its own test binary: the signal is
//! process-wide and would trim the caches of every other test's renderer.

use regia_render::{
    CacheResidency, DirectionalLightUniform, FrameMesh, FrameRenderer, FrameScene, LabMaterial, LabMesh,
    LabScene, LabVertex, RenderDevice, signal_memory_pressure,
};

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A small quad at `x`; `detail` extra vertices give each mesh its own bytes and key.
fn quad(x: f32, detail: usize) -> LabMesh {
    let mut vertices = vec![
        LabVertex::from_pos_normal_uv([x - 0.2, -0.2, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
        LabVertex::from_pos_normal_uv([x + 0.2, -0.2, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([x + 0.2, 0.2, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([x - 0.2, 0.2, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
    ];
    for i in 0..detail {
        let t = (i + 1) as f32 / (detail + 1) as f32;
        vertices.push(LabVertex::from_pos_normal_uv(
            [x - 0.2 + 0.4 * t, -0.2, 0.0],
            [0.0, 0.0, 1.0],
            [t, 1.0],
        ));
    }
    LabMesh {
        vertices,
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

fn scene() -> LabScene {
    LabScene {
        width: 16,
        height: 16,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 10.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: (0..4).map(|i| quad(i as f32 * 0.5 - 0.75, i)).collect(),
    }
}

fn with_slots<'a>(full: &FrameScene<'a>, slots: &[usize]) -> FrameScene<'a> {
    let meshes: Vec<FrameMesh<'a>> = slots.iter().map(|&slot| full.meshes[slot]).collect();
    FrameScene {
        meshes,
        ..full.clone()
    }
}

fn run(renderer: &mut FrameRenderer, device: &RenderDevice, scene: &FrameScene<'_>, frames: u64) {
    for _ in 0..frames {
        renderer.render_offscreen(device, scene).expect("frame");
    }
}

#[test]
fn memory_pressure_evicts_idle_entries_under_the_budget() {
    let device = device();
    let lab = scene();
    let full = lab.as_frame_scene();
    let mut renderer = FrameRenderer::new();
    // A budget nothing here comes near, and the 600-frame floor of the budget sweep.
    renderer.set_cache_residency(CacheResidency {
        budget_bytes: 1 << 40,
        ..CacheResidency::default()
    });
    // Unit 1 leaves first (idle 50 frames at the signal), unit 3 10 frames before it (idle 10),
    // unit 2 leaves with unit 1 but is pinned. No regular sweep runs (frame 256 is not reached).
    run(&mut renderer, &device, &with_slots(&full, &[0, 1, 2, 3]), 4);
    run(&mut renderer, &device, &with_slots(&full, &[0, 3]), 40);
    run(&mut renderer, &device, &with_slots(&full, &[0]), 10);
    renderer.pin_keys([full.meshes[2].mesh_key]);
    let before = renderer.cache_stats();
    assert_eq!(before.meshes, 4, "{before:?}");
    assert_eq!(before.meshes_evicted, 0, "{before:?}");
    signal_memory_pressure();
    run(&mut renderer, &device, &with_slots(&full, &[0]), 1);
    let after = renderer.cache_stats();
    eprintln!("before {before:?}\nafter {after:?}");
    assert_eq!(
        after.meshes_evicted, 1,
        "only the long-idle unpinned unit 1 goes: {after:?}"
    );
    assert_eq!(after.meshes, 3, "{after:?}");
    assert_eq!(
        after.materials_evicted, 0,
        "the shared material is drawn: {after:?}"
    );
    // Pinned unit 2 and recently drawn unit 3 draw without uploads; unit 1 uploads again.
    run(&mut renderer, &device, &with_slots(&full, &[0, 2, 3]), 1);
    assert_eq!(renderer.cache_stats().mesh_uploads, after.mesh_uploads);
    run(&mut renderer, &device, &with_slots(&full, &[0, 1, 2, 3]), 1);
    assert_eq!(renderer.cache_stats().mesh_uploads, after.mesh_uploads + 1);
    // One signal trims once: later frames are back on the budget.
    run(&mut renderer, &device, &with_slots(&full, &[0]), 60);
    assert_eq!(renderer.cache_stats().meshes_evicted, 1);
}
