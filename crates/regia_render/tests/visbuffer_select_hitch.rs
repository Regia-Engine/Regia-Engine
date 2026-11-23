// SPDX-License-Identifier: MPL-2.0
//! The hitch gate for the dense-scene selection (ADR-2071, `specs/27` § 2.4). One test in its own
//! binary, so the process-wide creation counters see only it.
//!
//! A warmed profile that selects by density compiles nothing on the frame the scene turns dense.
//! A profile that does not select by density asks the warm for nothing.

use std::sync::Arc;

use regia_render::pipeline_cache::pipelines_created;
use regia_render::pipeline_warm::canonical_frame;
use regia_render::visbuffer::{MeshletBuilder, MeshletMesh};
use regia_render::{
    DenseSceneSelect, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex, PostSettings,
    RenderDevice, VisibilityPath, VisibilitySelect, box_mesh,
};

fn single_level_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        Some(MeshletMesh::single_level(&positions, indices))
    })
}

fn scene(width: u32, height: u32, count: usize) -> LabScene {
    let (vertices, indices) = box_mesh([0.4, 0.4, 0.4]);
    let mut meshes = Vec::new();
    for i in 0..count {
        let mut model = [[0.0f32; 4]; 4];
        for (axis, row) in model.iter_mut().enumerate() {
            row[axis] = 1.0;
        }
        model[3] = [i as f32 - count as f32 * 0.5, 0.0, 0.0, 1.0];
        meshes.push(LabMesh {
            vertices: vertices.clone(),
            indices: indices.clone(),
            material: 0,
            model,
            ..LabMesh::unskinned()
        });
    }
    LabScene {
        width,
        height,
        eye: [0.0, 6.0, 10.0],
        target: [0.0; 3],
        fov_y: 1.0,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(PostSettings::golden_deterministic()),
        directional: regia_render::DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes,
    }
}

/// One run of the gate on `device`.
fn check(device: &RenderDevice, single_pass_resolve: bool) {
    let (w, h) = (160, 96);
    let select = DenseSceneSelect {
        enter_triangles: u64::MAX,
        enter_instances: 16,
        enter_frames: 2,
        leave_frames: 2,
    };
    let path = VisibilityPath {
        select: VisibilitySelect::Dense(select),
        single_pass_resolve,
    };

    // What `GameFrameWarm` runs, at the window size, with the frame's HDR target.
    let mut frame = canonical_frame();
    frame.quality.msaa_samples = 1;
    frame.quality.visibility_buffer = true;
    frame.quality.visibility_path = path;
    frame.depth_prepass = true;
    let warmed = |target: &mut FrameRenderer| {
        target.set_meshlet_builder(Some(single_level_builder()));
        target
            .warm_extracted_at(
                device,
                &frame,
                wgpu::TextureFormat::Rgba8Unorm,
                true,
                Some((w, h)),
            )
            .expect("warm");
    };
    let mut renderer = FrameRenderer::new();
    warmed(&mut renderer);

    let mut quality = FrameQuality::LAB;
    quality.visibility_buffer = true;
    quality.visibility_path = path;

    // Sparse: the classic frame.
    let sparse = scene(w, h, 2);
    let sparse_frame = sparse.as_frame_scene_with_quality(quality);
    renderer.render_offscreen(device, &sparse_frame).expect("sparse");
    renderer.render_offscreen(device, &sparse_frame).expect("sparse");
    assert!(!renderer.visbuffer_active());

    // The scene turns dense. The warm built the path's pipelines: none compiles here.
    let dense = scene(w, h, 32);
    let dense_frame = dense.as_frame_scene_with_quality(quality);
    let before = pipelines_created();
    for _ in 0..6 {
        renderer.render_offscreen(device, &dense_frame).expect("dense");
    }
    assert!(renderer.visbuffer_active(), "the scene is dense");
    assert_eq!(
        renderer.visbuffer_resolve_report().single_pass,
        single_pass_resolve,
        "{:?}",
        renderer.visbuffer_resolve_report()
    );
    assert_eq!(
        pipelines_created() - before,
        0,
        "the frames the scene turned dense compiled pipelines"
    );

    // And back: the classic pipelines stayed.
    let before = pipelines_created();
    for _ in 0..4 {
        renderer.render_offscreen(device, &sparse_frame).expect("sparse");
    }
    assert!(!renderer.visbuffer_active(), "the scene is sparse again");
    assert_eq!(pipelines_created() - before, 0, "going back compiled pipelines");

    // The control: the same switch with no warm compiles on the frame path, so the zero above
    // is the warm's doing and the count can see it.
    let mut cold = FrameRenderer::new();
    cold.set_meshlet_builder(Some(single_level_builder()));
    cold.render_offscreen(device, &sparse_frame).expect("cold sparse");
    cold.render_offscreen(device, &sparse_frame).expect("cold sparse");
    let before = pipelines_created();
    for _ in 0..6 {
        cold.render_offscreen(device, &dense_frame).expect("cold dense");
    }
    assert!(cold.visbuffer_active());
    assert!(
        pipelines_created() - before > 0,
        "without the warm the switch must compile the path's pipelines"
    );

    // An abandoned or worker warm hands its path pipelines over like the forward sets.
    let mut abandoned = FrameRenderer::new();
    warmed(&mut abandoned);
    let mut stand_in = FrameRenderer::new();
    stand_in.set_meshlet_builder(Some(single_level_builder()));
    assert!(stand_in.adopt_warm_pipelines(&mut abandoned));
    for _ in 0..6 {
        stand_in.render_offscreen(device, &dense_frame).expect("stand-in");
    }
    assert!(stand_in.visbuffer_active());
    assert_eq!(
        stand_in.visbuffer_create_stats().total.pipelines,
        0,
        "the stand-in took the path's pipelines"
    );

    // Without the selection the new warm key asks for nothing.
    let mut off = FrameRenderer::new();
    let plain = sparse.as_frame_scene_with_quality(FrameQuality::LAB);
    off.render_offscreen(device, &plain).expect("plain");
    assert_eq!(off.warm_pipeline_key_for_tests(device, "visbuffer_path"), 0);
}

#[test]
fn a_warmed_dense_selection_compiles_nothing_when_the_scene_turns_dense() {
    let device = RenderDevice::headless().expect("device");
    check(&device, false);
    // The single pass, on a device with the texture table.
    regia_render::texture_table::set_device_level_request(regia_render::texture_table::TEXTURE_TABLE_LEVEL);
    let table = RenderDevice::headless().expect("table device");
    regia_render::texture_table::set_device_level_request(0);
    if regia_render::texture_table::device_support(&table.device)
        .capacity()
        .is_some()
    {
        check(&table, true);
    } else {
        eprintln!("skip the single pass: this adapter stays off the texture table");
    }
}
