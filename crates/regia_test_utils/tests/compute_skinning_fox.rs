// SPDX-License-Identifier: MPL-2.0
//! Compute skinning on the Khronos Fox (Tincal B2): the compute path skins the Fox in its
//! "Walk" pose like the vertex shaders do, vertex by vertex and in the rendered frame.
//!
//! Skips when the Fox sample is not available (as `golden_skinned_fox` does).

use regia_render::{
    FrameRenderer, RenderDevice, SkinningMethod, SkinningOptions, skin_on_gpu, skin_vertex_reference,
};
use regia_test_utils::scene_loader::{LabView, ensure_model, load_gltf_model_with_view};

#[test]
fn compute_skinning_matches_the_vertex_path_on_the_fox() {
    let path = match ensure_model("Fox") {
        Ok(p) => p,
        Err(err) => {
            eprintln!("skip compute_skinning_fox: {err}");
            return;
        }
    };
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip compute_skinning_fox: {err}");
            return;
        }
    };
    let mut scene = match load_gltf_model_with_view(&path, LabView::ModelPortrait, Some(("Walk", 0.5))) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("skip compute_skinning_fox: load failed: {err}");
            return;
        }
    };
    scene.width = 256;
    scene.height = 256;
    let skinned: Vec<_> = scene.meshes.iter().filter(|m| !m.joints.is_empty()).collect();
    assert!(!skinned.is_empty(), "the Fox has no skinned mesh");

    // Vertex by vertex: the kernel against the CPU reference of the vertex shaders' blend.
    for mesh in &skinned {
        let gpu = skin_on_gpu(&device, &mesh.vertices, &mesh.joints, &SkinningOptions::compute())
            .expect("compute skinning");
        let extent = mesh
            .vertices
            .iter()
            .flat_map(|v| v.position)
            .fold(0.0f32, |a, c| a.max(c.abs()))
            .max(1.0);
        let mut error = 0.0f32;
        let mut moved = 0.0f32;
        for (g, v) in gpu.iter().zip(&mesh.vertices) {
            let influences: Vec<(u32, f32)> = (0..4).map(|k| (v.joints[k], v.weights[k])).collect();
            let tangent = [v.tangent[0], v.tangent[1], v.tangent[2]];
            let (p, n, _) = skin_vertex_reference(
                (v.position, v.normal, tangent),
                &influences,
                &mesh.joints,
                SkinningMethod::Linear,
            );
            for k in 0..3 {
                error = error.max((g.position[k] - p[k]).abs() / extent);
                error = error.max((g.normal[k] - n[k]).abs());
                moved = moved.max((g.position[k] - v.position[k]).abs() / extent);
            }
        }
        eprintln!(
            "Fox: {} vertices, {} joints: relative error {error:e}, pose moved {moved:.3} of the extent",
            mesh.vertices.len(),
            mesh.joints.len()
        );
        assert!(
            error < 1e-5,
            "the kernel differs from the vertex shaders' blend ({error})"
        );
        assert!(moved > 0.01, "the Walk pose did not move the Fox");
    }

    // The frame: shadows, depth prepass and colour from the compute twin.
    let frame = scene.as_frame_scene();
    let mut vertex_path = FrameRenderer::new();
    let mut compute = FrameRenderer::new();
    compute.set_compute_skinning(true);
    let a = vertex_path
        .render_offscreen(&device, &frame)
        .expect("vertex path");
    let b = compute.render_offscreen(&device, &frame).expect("compute path");
    let stats = compute.skinning_stats();
    assert_eq!(stats.instances, skinned.len(), "{stats:?}");
    let mut over = 0usize;
    let mut fox = 0usize;
    for (pa, pb) in a
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.pixels.as_chunks::<4>().0)
    {
        if (0..3).any(|k| pa[k] > 12) {
            fox += 1;
        }
        if (0..3).any(|k| pa[k].abs_diff(pb[k]) > 3) {
            over += 1;
        }
    }
    eprintln!("Fox frame: {fox} lit px, {over} differ by more than 3 levels; {stats:?}");
    assert!(fox > 4000, "the Fox is not in view ({fox} px)");
    assert!(
        over <= fox / 200,
        "the compute path drew the Fox differently: {over} of {fox} px"
    );
}
