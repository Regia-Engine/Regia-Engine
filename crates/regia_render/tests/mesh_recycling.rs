// SPDX-License-Identifier: MPL-2.0
//! Mesh buffer recycling (roadmap item 7a): generation mesh keys and the mesh arenas.
//!
//! A host that replaces its mesh `Arc`s every frame (new `Arc`, same or new content) re-uploads
//! each frame into ranges the dropped meshes freed: no mesh buffer is created after warm-up. A
//! new `Arc` never inherits a dropped one's key, even at the same address and size (the ABA
//! hole of address keys), and an evicted mesh's range goes to the next upload.
//!
//! One GPU test in this binary: the upload counters, the released-key queue and the memory
//! pressure signal are process-wide.

use std::sync::Arc;

use regia_render::{
    CpuMesh, DirectionalLightUniform, FrameMesh, FrameRenderer, FrameScene, LabMaterial, LabMesh, LabScene,
    LabVertex, RenderDevice, mesh_key, signal_memory_pressure, take_released_mesh_keys, upload_split,
};

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A quad at `x` with `detail` extra vertices on its bottom edge (its own byte size).
fn quad(x: f32, detail: usize) -> CpuMesh {
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
    CpuMesh {
        vertices,
        indices: vec![0, 1, 2, 0, 2, 3],
    }
}

/// The lab frame the meshes are drawn in (camera, light, one material).
fn lab() -> LabScene {
    LabScene {
        width: 32,
        height: 32,
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
        meshes: vec![LabMesh {
            vertices: quad(0.0, 0).vertices,
            indices: vec![0, 1, 2, 0, 2, 3],
            ..LabMesh::unskinned()
        }],
    }
}

/// `base` drawing `meshes` (one object each) under their [`mesh_key`]s, as the runtime does.
fn frame<'a>(base: &FrameScene<'a>, meshes: &'a [Arc<CpuMesh>]) -> FrameScene<'a> {
    let template = base.meshes[0];
    FrameScene {
        meshes: meshes
            .iter()
            .enumerate()
            .map(|(i, mesh)| FrameMesh {
                mesh_key: mesh_key(mesh),
                object_key: 0x0B1E_C700 + i as u64,
                vertices: &mesh.vertices,
                indices: &mesh.indices,
                ..template
            })
            .collect(),
        ..base.clone()
    }
}

fn units(count: usize) -> Vec<Arc<CpuMesh>> {
    (0..count)
        .map(|i| Arc::new(quad(i as f32 * 0.5 - 0.75, i)))
        .collect()
}

#[test]
fn replaced_arcs_reuse_ranges_new_arcs_never_inherit_keys_and_evictions_free_ranges() {
    let device = device();
    let lab = lab();
    let base = lab.as_frame_scene();

    // 1. A host that builds new `Arc`s every frame (same content): every frame re-uploads,
    //    into the ranges the previous frame's meshes freed.
    let mut renderer = FrameRenderer::new();
    const WARM: usize = 4;
    const FRAMES: usize = 64;
    let mut warm = None;
    for n in 0..FRAMES {
        renderer.release_meshes(&take_released_mesh_keys());
        let meshes = units(4);
        renderer
            .render_offscreen(&device, &frame(&base, &meshes))
            .expect("frame");
        drop(meshes);
        if n + 1 == WARM {
            warm = Some((
                upload_split(),
                renderer.mesh_arena_stats(),
                renderer.cache_stats(),
            ));
        }
    }
    renderer.release_meshes(&take_released_mesh_keys());
    let (split_warm, arenas_warm, stats_warm) = warm.expect("warm-up");
    let split = upload_split();
    let arenas = renderer.mesh_arena_stats();
    let stats = renderer.cache_stats();
    eprintln!("warm {split_warm:?} {arenas_warm:?} {stats_warm:?}\nend  {split:?} {arenas:?} {stats:?}");
    assert_eq!(
        split.mesh_buffers, split_warm.mesh_buffers,
        "no mesh buffer created after warm-up"
    );
    assert_eq!(arenas.0.chunks, arenas_warm.0.chunks, "vertex arena: {arenas:?}");
    assert_eq!(arenas.1.chunks, arenas_warm.1.chunks, "index arena: {arenas:?}");
    assert_eq!(
        stats.mesh_uploads - stats_warm.mesh_uploads,
        4 * (FRAMES - WARM) as u64,
        "every new Arc uploads once: {stats:?}"
    );
    assert_eq!(stats.meshes, 0, "every dropped Arc was released: {stats:?}");
    assert!(stats.meshes_released >= 4 * (FRAMES - 1) as u64, "{stats:?}");
    assert_eq!(
        arenas.0.free_bytes, arenas.0.bytes,
        "all vertex ranges free again: {arenas:?}"
    );
    assert_eq!(arenas.0.free_ranges, arenas.0.chunks, "coalesced: {arenas:?}");

    // 2. ABA: drop a mesh, then draw a different mesh of the same byte size (the allocator may
    //    well hand it the same address). It must draw its own content, as on a fresh renderer.
    let mut renderer = FrameRenderer::new();
    let a = vec![Arc::new(quad(-0.5, 2))];
    let key_a = mesh_key(&a[0]);
    let image_a = renderer
        .render_offscreen(&device, &frame(&base, &a))
        .expect("frame a");
    drop(a);
    renderer.release_meshes(&take_released_mesh_keys());
    let b = vec![Arc::new(quad(0.5, 2))];
    assert_ne!(mesh_key(&b[0]), key_a, "a new Arc never reuses a dropped key");
    let image_b = renderer
        .render_offscreen(&device, &frame(&base, &b))
        .expect("frame b");
    let fresh_b = FrameRenderer::new()
        .render_offscreen(&device, &frame(&base, &b))
        .expect("fresh b");
    assert_ne!(image_a.pixels, image_b.pixels, "the quad moved");
    assert_eq!(image_b.pixels, fresh_b.pixels, "b draws its own vertices");
    // The same Arc (and its clones) keeps its key.
    let b2 = Arc::clone(&b[0]);
    assert_eq!(mesh_key(&b2), mesh_key(&b[0]));

    // 3. Eviction frees ranges: units 1-3 idle, memory pressure evicts them; a new mesh no
    //    larger than they were takes their space without a buffer.
    let mut renderer = FrameRenderer::new();
    let meshes = units(4);
    renderer
        .render_offscreen(&device, &frame(&base, &meshes))
        .expect("frame");
    for _ in 0..40 {
        renderer
            .render_offscreen(&device, &frame(&base, &meshes[..1]))
            .expect("frame");
    }
    let before = renderer.mesh_arena_stats();
    signal_memory_pressure();
    renderer
        .render_offscreen(&device, &frame(&base, &meshes[..1]))
        .expect("frame");
    let evicted = renderer.mesh_arena_stats();
    let stats = renderer.cache_stats();
    eprintln!("before {before:?}\nevicted {evicted:?} {stats:?}");
    assert_eq!(stats.meshes_evicted, 3, "{stats:?}");
    let freed: u64 = (1..4)
        .map(|i| (meshes[i].vertices.len() * std::mem::size_of::<LabVertex>()) as u64)
        .sum();
    assert_eq!(evicted.0.free_bytes, before.0.free_bytes + freed, "{evicted:?}");
    assert_eq!(
        evicted.1.free_bytes,
        before.1.free_bytes + 3 * 6 * 4,
        "{evicted:?}"
    );
    let split = upload_split();
    let newcomer = vec![meshes[0].clone(), Arc::new(quad(0.25, 3))];
    renderer
        .render_offscreen(&device, &frame(&base, &newcomer))
        .expect("frame");
    assert_eq!(
        upload_split().mesh_buffers,
        split.mesh_buffers,
        "the newcomer reused a range"
    );
    assert_eq!(renderer.mesh_arena_stats().0.chunks, evicted.0.chunks);
}
