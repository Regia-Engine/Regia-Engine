// SPDX-License-Identifier: MPL-2.0
//! The cluster light-index buffer starts at the frame's lights-per-cluster cap and grows by
//! doubling when a frame asks for more (ADR-3351); it used to be made at the 256-light maximum
//! (3.4 MiB) for every profile. One test in its own binary: the creation counters are
//! process-wide.

use regia_render::{
    DirectionalLightUniform, FrameMesh, FramePathKind, FrameQuality, FrameRenderer, FrameScene,
    LabLocalLight, LabMaterial, LabMesh, LabPointLight, LabScene, LabVertex, RenderDevice,
    cluster_index_bytes, frame_path_counters_all_threads as process_counters, take_frame_path_events,
    trace_frame_path,
};

/// Buffer bytes the first frame of a one-quad 64x64 Lab scene may create: 6,197,744 B measured on
/// lavapipe (llvmpipe, LLVM 23.1.4) with the index buffer at the Lab cap (32 lights per cluster,
/// 442,368 B), plus 5 %. With the buffer at the 256-light maximum the same frame creates
/// 9,294,320 B and fails.
const ONE_QUAD_BUFFER_BOUND: u64 = 6_197_744 + 6_197_744 / 20;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn quad(half: f32) -> LabMesh {
    LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-half, -half, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([half, -half, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([half, half, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([-half, half, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

/// One 2 m quad facing the camera, lit by `lights` small point lights packed in front of it so
/// every froxel over the quad lists all of them.
fn lab(lights: usize) -> LabScene {
    let local_lights = (0..lights)
        .map(|i| {
            let a = i as f32 * 0.37;
            LabLocalLight::Point(LabPointLight {
                position: [0.3 * a.cos(), 0.3 * a.sin(), 0.4],
                color: [1.0, 0.6 + 0.01 * i as f32, 0.4],
                intensity: 60.0,
                radius: 3.0,
                cast_shadows: false,
                source_radius: None,
            })
        })
        .collect();
    LabScene {
        width: 64,
        height: 64,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 10.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 8.0,
        ambient_lux: 500.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 2_000.0),
        local_lights,
        materials: vec![LabMaterial::default()],
        meshes: vec![quad(1.0)],
    }
}

fn with_cap(scene: &LabScene, cap: u32) -> FrameScene<'_> {
    let mut quality = FrameQuality::LAB;
    quality.max_lights_per_cluster = cap;
    scene.as_frame_scene_with_quality(quality)
}

/// FNV-1a of a frame's pixels (for messages; the comparison is on the bytes).
fn hash(pixels: &[u8]) -> u64 {
    pixels.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The cluster index buffers this thread created since the last call, by size (the frame makes
/// its buffers on the calling thread).
fn cluster_index_creates() -> Vec<u64> {
    take_frame_path_events()
        .into_iter()
        .filter(|e| e.kind == FramePathKind::Buffer && e.label == "cluster indices")
        .map(|e| e.bytes)
        .collect()
}

#[test]
fn the_cluster_index_buffer_starts_at_the_cap_grows_on_demand_and_renders_the_same() {
    let device = device();
    trace_frame_path(true);

    // 1. One quad: the first frame's buffers stay under the bound, and the index buffer is the
    //    Lab cap's, not the maximum's.
    let one = lab(0);
    let before = process_counters();
    let _ = take_frame_path_events();
    let mut renderer = FrameRenderer::new();
    renderer
        .render_offscreen(&device, &one.as_frame_scene())
        .expect("one-quad frame");
    let created = process_counters().buffer_bytes - before.buffer_bytes;
    eprintln!("one quad: {created} B of buffers on the first frame");
    assert_eq!(
        cluster_index_creates(),
        vec![cluster_index_bytes(FrameQuality::LAB.max_lights_per_cluster)],
        "the index buffer is sized for the Lab cap"
    );
    assert!(
        created <= ONE_QUAD_BUFFER_BOUND,
        "the one-quad frame created {created} B of buffers, over {ONE_QUAD_BUFFER_BOUND} B"
    );

    // 2. Forty lights over the quad: a renderer that started at 16 lights per cluster and then
    //    draws at 64 grows its buffer once and draws what a renderer that started at 64 draws.
    let lit = lab(40);
    let mut grown = FrameRenderer::new();
    let small = grown
        .render_offscreen(&device, &with_cap(&lit, 16))
        .expect("cap 16");
    let _ = take_frame_path_events();
    let after_growth = grown
        .render_offscreen(&device, &with_cap(&lit, 64))
        .expect("cap 64");
    assert_eq!(
        cluster_index_creates(),
        vec![cluster_index_bytes(64)],
        "one larger buffer"
    );
    let mut fresh = FrameRenderer::new();
    let _ = fresh
        .render_offscreen(&device, &with_cap(&lit, 64))
        .expect("fresh cap 64");
    let reference = fresh
        .render_offscreen(&device, &with_cap(&lit, 64))
        .expect("fresh cap 64");
    assert_ne!(
        hash(&small.pixels),
        hash(&reference.pixels),
        "16 of 40 lights per cluster must look different, or the test proves nothing"
    );
    assert!(
        after_growth.pixels == reference.pixels,
        "grown {:016x} vs fresh {:016x}",
        hash(&after_growth.pixels),
        hash(&reference.pixels)
    );
    // Lower caps later keep the larger buffer.
    let _ = take_frame_path_events();
    let again = grown
        .render_offscreen(&device, &with_cap(&lit, 16))
        .expect("cap 16 again");
    assert!(cluster_index_creates().is_empty());
    assert!(
        again.pixels == small.pixels,
        "cap 16 draws the same with the larger buffer"
    );

    // 3. 10,000 instances of the quad: a renderer whose per-instance buffers start at the runtime
    //    reserve and grow draws what one reserved for all of them up front draws.
    let tile = lab(4);
    let base = tile.as_frame_scene();
    let template: FrameMesh<'_> = base.meshes[0];
    let meshes: Vec<FrameMesh<'_>> = (0..10_000u32)
        .map(|i| {
            let (x, y) = ((i % 100) as f32, (i / 100) as f32);
            let mut mesh = template;
            mesh.object_key = 0x5EED_0000 + u64::from(i);
            mesh.model = [
                [0.012, 0.0, 0.0, 0.0],
                [0.0, 0.012, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [(x - 49.5) * 0.024, (y - 49.5) * 0.024, 0.0, 1.0],
            ];
            mesh
        })
        .collect();
    let crowd = FrameScene {
        meshes,
        ..base.clone()
    };
    let mut growing = FrameRenderer::new();
    let grown_frame = growing.render_offscreen(&device, &crowd).expect("10k, growing");
    let mut reserved = FrameRenderer::new();
    reserved.reserve_instances(10_000);
    let reserved_frame = reserved.render_offscreen(&device, &crowd).expect("10k, reserved");
    assert!(
        grown_frame.pixels == reserved_frame.pixels,
        "10,000 instances: growing {:016x} vs reserved {:016x}",
        hash(&grown_frame.pixels),
        hash(&reserved_frame.pixels)
    );
    eprintln!("10k instances: {:016x}", hash(&grown_frame.pixels));
}
