// SPDX-License-Identifier: MPL-2.0
//! Density thresholds from counters (ADR-2689): [`DensityMetrics`] measured on real frames, and a
//! threshold chosen in those units selects the denser scene and not the sparser one. Counters only, no timing: the crossover itself comes from the hardware bench
//! (`visbuffer_dense_bench`, `book/agents/visbuffer.md`).

use std::sync::Arc;

use regia_render::visbuffer::{DensityMetrics, MeshletBuilder, MeshletMesh, SceneDensity};
use regia_render::{
    DenseSceneSelect, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, LabVertex, PostSettings, RenderDevice, VisibilityPath, VisibilitySelect, uv_sphere,
};

fn single_level_builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        Some(MeshletMesh::single_level(&positions, indices))
    })
}

fn lab(width: u32, height: u32) -> LabScene {
    let (vertices, indices) = uv_sphere(0.45, 24, 12);
    LabScene {
        width,
        height,
        eye: [0.0, 5.0, 6.0],
        target: [0.0, 0.0, -6.0],
        fov_y: 1.0,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: false,
        post: Some(PostSettings::golden_deterministic()),
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 60_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![LabMesh {
            vertices,
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

/// `instances` copies of the sphere on a grid in front of the camera; the frame's metrics after
/// three frames through the visibility path (`select`), and whether the last one took it.
fn measure(
    device: &RenderDevice,
    (width, height): (u32, u32),
    instances: u32,
    select: VisibilitySelect,
) -> (DensityMetrics, bool) {
    let lab = lab(width, height);
    let mut quality = FrameQuality::LAB;
    quality.visibility_buffer = true;
    quality.visibility_path = VisibilityPath {
        select,
        single_pass_resolve: false,
    };
    let mut scene = lab.as_frame_scene_with_quality(quality);
    let template = scene.meshes[0];
    let side = f64::from(instances).sqrt().ceil() as u32;
    scene.meshes = (0..instances)
        .map(|i| {
            let mut model = [[0.0f32; 4]; 4];
            for (axis, row) in model.iter_mut().enumerate() {
                row[axis] = 1.0;
            }
            model[3] = [
                (i % side) as f32 * 1.1 - side as f32 * 0.55,
                0.5,
                -((i / side) as f32) * 1.1,
                1.0,
            ];
            FrameMesh {
                object_key: 1000 + u64::from(i),
                model,
                ..template
            }
        })
        .collect();
    let mut renderer = FrameRenderer::new();
    renderer.set_meshlet_builder(Some(single_level_builder()));
    for _ in 0..3 {
        renderer.render_offscreen(device, &scene).expect("frame");
    }
    let on_path = renderer.visbuffer_active();
    let counters = if on_path {
        renderer.visbuffer_counters(device).expect("readback")
    } else {
        None
    };
    let density = SceneDensity {
        triangles: (template.indices.len() / 3) as u64 * u64::from(instances),
        instances,
    };
    (
        DensityMetrics::of_frame(density, counters, width, height),
        on_path,
    )
}

#[test]
fn a_threshold_chosen_from_counters_selects_the_denser_scene() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip: {err}");
            return;
        }
    };
    let small = (128, 72);
    let (sparse, _) = measure(&device, small, 4, VisibilitySelect::Always);
    let (dense, _) = measure(&device, small, 64, VisibilitySelect::Always);
    println!("sparse {sparse:?}\ndense {dense:?}");
    assert!(dense.triangles_per_pixel > sparse.triangles_per_pixel * 8.0);
    assert!(
        sparse.clusters_per_tile > 0.0,
        "the path drew clusters: {sparse:?}"
    );
    assert!(
        dense.clusters_per_tile > sparse.clusters_per_tile,
        "more geometry on screen, more clusters per tile: {sparse:?} vs {dense:?}"
    );
    // A threshold between the two, in density units, turned into the gate's counts for this
    // size: the denser scene enters, the sparser does not.
    let tpp = (sparse.triangles_per_pixel * dense.triangles_per_pixel).sqrt();
    let select = VisibilitySelect::Dense(DenseSceneSelect {
        enter_frames: 2,
        ..DenseSceneSelect::at_density(tpp, f64::NAN, small.0, small.1)
    });
    let (_, sparse_on) = measure(&device, small, 4, select);
    let (_, dense_on) = measure(&device, small, 64, select);
    assert!(!sparse_on, "the sparse scene took the path");
    assert!(dense_on, "the dense scene stayed on the classic path");
}
