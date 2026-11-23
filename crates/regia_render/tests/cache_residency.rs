// SPDX-License-Identifier: MPL-2.0
//! Mesh and material cache residency (`CacheResidency`, DO27 PERF_JANK item 6).
//!
//! A unit that leaves the view (the host stops submitting it) and comes back 20 s later draws
//! without re-uploading its buffers or textures while the caches fit their budget. Over the
//! budget the sweep evicts the least recently drawn entries first, and pinned keys stay.

use regia_render::{
    CacheResidency, DirectionalLightUniform, FrameMesh, FrameRenderer, FrameScene, LabMaterial, LabMesh,
    LabScene, LabVertex, RenderDevice,
};

/// Frames at 60 Hz in 20 s.
const TWENTY_SECONDS: u64 = 1200;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A small quad at `x`, facing the camera; `detail` vertices on its bottom edge make each
/// mesh's bytes (and so its cache key) its own.
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

/// Four quads: slot 0 is always drawn, slots 1–3 are the units that come and go.
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

/// `full` with only the meshes of `slots`.
fn with_slots<'a>(full: &FrameScene<'a>, slots: &[usize]) -> FrameScene<'a> {
    let meshes: Vec<FrameMesh<'a>> = slots.iter().map(|&slot| full.meshes[slot]).collect();
    FrameScene {
        meshes,
        ..full.clone()
    }
}

/// Renders `frames` frames of `scene`.
fn run(renderer: &mut FrameRenderer, device: &RenderDevice, scene: &FrameScene<'_>, frames: u64) {
    for _ in 0..frames {
        renderer.render_offscreen(device, scene).expect("frame");
    }
}

fn mesh_bytes(mesh: &FrameMesh<'_>) -> u64 {
    (std::mem::size_of_val(mesh.vertices) + std::mem::size_of_val(mesh.indices)) as u64
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_unit_back_after_20_seconds_uploads_nothing_under_budget() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene();
    let full = lab.as_frame_scene();
    let all = with_slots(&full, &[0, 1]);
    let alone = with_slots(&full, &[0]);
    let mut renderer = FrameRenderer::new();
    run(&mut renderer, &device, &all, 4);
    let before = renderer.cache_stats();
    assert_eq!(before.meshes, 2, "{before:?}");
    // The unit leaves the view for 20 s (several sweeps pass) and comes back.
    run(&mut renderer, &device, &alone, TWENTY_SECONDS);
    let idle = renderer.cache_stats();
    run(&mut renderer, &device, &all, 4);
    let after = renderer.cache_stats();
    eprintln!("before {before:?}\nidle {idle:?}\nafter {after:?}");
    assert!(
        idle.resident_bytes > 0 && idle.resident_bytes <= idle.budget_bytes,
        "{idle:?}"
    );
    assert_eq!(
        idle.meshes, 2,
        "the idle unit stays resident under the budget: {idle:?}"
    );
    assert_eq!(
        after.mesh_uploads, before.mesh_uploads,
        "no mesh re-upload: {after:?}"
    );
    assert_eq!(
        after.material_uploads, before.material_uploads,
        "no material re-upload: {after:?}"
    );
    assert_eq!(after.meshes_evicted, 0, "{after:?}");
    // The old policy (every unpinned entry idle 600 frames evicted) is a budget always
    // exceeded: the same 20 s away re-uploads the unit's mesh.
    let mut aged = FrameRenderer::new();
    aged.set_cache_residency(CacheResidency {
        budget_bytes: 1,
        ..CacheResidency::default()
    });
    run(&mut aged, &device, &all, 4);
    let before = aged.cache_stats();
    run(&mut aged, &device, &alone, TWENTY_SECONDS);
    run(&mut aged, &device, &all, 4);
    let after = aged.cache_stats();
    eprintln!("age policy: before {before:?}\nafter {after:?}");
    assert_eq!(after.mesh_uploads, before.mesh_uploads + 1, "{after:?}");
}

#[test]
fn over_budget_the_least_recently_drawn_go_first_and_pins_stay() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene();
    let full = lab.as_frame_scene();
    let mut renderer = FrameRenderer::new();
    // Units 1, 2, 3 leave the view in the order 3, 2, 1 (3 is the least recently drawn).
    run(&mut renderer, &device, &with_slots(&full, &[0, 1, 2, 3]), 10);
    run(&mut renderer, &device, &with_slots(&full, &[0, 1, 2]), 10);
    run(&mut renderer, &device, &with_slots(&full, &[0, 1]), 10);
    let alone = with_slots(&full, &[0]);
    // The first sweep (frame 256) measures; nothing is idle 600 frames yet.
    run(&mut renderer, &device, &alone, 256 - 30);
    let measured = renderer.cache_stats();
    assert_eq!(measured.meshes, 4, "{measured:?}");
    assert_eq!(measured.meshes_evicted, 0, "{measured:?}");
    // A budget one and a half unit meshes short: unpinned, the sweep would evict units 3 and 2
    // (the two least recently drawn) and keep unit 1.
    let budget = measured.resident_bytes - mesh_bytes(&full.meshes[3]) - mesh_bytes(&full.meshes[2]) / 2;
    assert!(budget > 0);
    renderer.set_cache_residency(CacheResidency {
        budget_bytes: budget,
        ..CacheResidency::default()
    });
    // Unit 2 pinned: the sweep has to take unit 1 after unit 3 instead.
    renderer.pin_keys([full.meshes[2].mesh_key]);
    // Through the sweep at frame 768 (units idle ≥ 600 frames by then).
    run(&mut renderer, &device, &alone, 768 - 256 + 1);
    let swept = renderer.cache_stats();
    eprintln!("measured {measured:?}\nswept {swept:?}");
    assert_eq!(swept.meshes_evicted, 2, "{swept:?}");
    assert_eq!(swept.meshes, 2, "{swept:?}");
    assert!(swept.resident_bytes <= budget, "{swept:?}");
    // The pinned unit 2 is still resident; units 3 and 1 upload again.
    let uploads = swept.mesh_uploads;
    run(&mut renderer, &device, &with_slots(&full, &[0, 2]), 1);
    assert_eq!(
        renderer.cache_stats().mesh_uploads,
        uploads,
        "pinned unit 2 stayed"
    );
    run(&mut renderer, &device, &with_slots(&full, &[0, 1, 2, 3]), 1);
    assert_eq!(
        renderer.cache_stats().mesh_uploads,
        uploads + 2,
        "units 1 and 3 were evicted"
    );
}

#[test]
fn the_age_cap_still_evicts_very_old_entries() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = scene();
    let full = lab.as_frame_scene();
    let mut renderer = FrameRenderer::new();
    renderer.set_cache_residency(CacheResidency {
        max_idle_frames: 300,
        ..CacheResidency::default()
    });
    run(&mut renderer, &device, &with_slots(&full, &[0, 1]), 4);
    // Past the sweep at frame 512: unit 1 idle > 300 frames, evicted under the budget.
    run(&mut renderer, &device, &with_slots(&full, &[0]), 512);
    let stats = renderer.cache_stats();
    eprintln!("{stats:?}");
    assert_eq!(stats.meshes_evicted, 1, "{stats:?}");
    assert_eq!(stats.meshes, 1, "{stats:?}");
}
