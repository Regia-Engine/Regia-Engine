// SPDX-License-Identifier: MPL-2.0
//! The forward shader's permutations stay inside its declared feature axes (ADR-1552).
//!
//! Renders a small lit scene through the CPU, GPU-driven and MSAA paths, then reads
//! `forward_permutation_report`: every define the forward shader was composed with is a declared
//! axis, and the count stays under the budget. The count is printed for the report.

use regia_render::{
    DirectionalLightUniform, FORWARD_AXES, FORWARD_PERMUTATION_BUDGET, FrameQuality, FrameRenderer,
    LabMaterial, LabMesh, LabScene, LabVertex, RenderDevice, forward_permutation_report,
};

fn lab() -> LabScene {
    let quad = |x: f32, cutout: bool| LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([x - 0.5, -0.5, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.5, -0.5, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.5, 0.5, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x - 0.5, 0.5, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        material: usize::from(cutout),
        ..LabMesh::unskinned()
    };
    LabScene {
        width: 32,
        height: 24,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.4, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.7, 0.6, 0.5, 1.0],
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.3, 0.6, 0.5, 0.4],
                alpha_mode: regia_render::ALPHA_MASK,
                ..LabMaterial::default()
            },
        ],
        meshes: vec![quad(-0.6, false), quad(0.6, true)],
    }
}

#[test]
fn composed_forward_permutations_use_only_declared_axes_and_fit_the_budget() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let lab = lab();
    for quality in [
        FrameQuality::default(),
        FrameQuality {
            gpu_driven: true,
            ..FrameQuality::default()
        },
        FrameQuality {
            msaa_samples: 4,
            ..FrameQuality::default()
        },
    ] {
        let mut renderer = FrameRenderer::new();
        renderer
            .render_offscreen(&device, &lab.as_frame_scene_with_quality(quality))
            .expect("frame");
    }
    let report = forward_permutation_report();
    eprintln!(
        "forward permutations composed: {} (budget {}, {} axes)",
        report.permutations.len(),
        report.budget,
        FORWARD_AXES.len()
    );
    for permutation in &report.permutations {
        eprintln!("  {permutation:?}");
    }
    assert!(
        !report.permutations.is_empty(),
        "the frames composed the forward shader"
    );
    // The GPU-driven frame's colour pipeline composes the forward shader by its file name, so
    // it is counted (and a dev edit of the file reaches it): before ADR-3145 it went by its
    // pipeline label and this report never saw it.
    assert!(
        report
            .permutations
            .iter()
            .any(|p| p.iter().any(|d| d == "GPU_DRIVEN")),
        "the GPU-driven frame's forward permutation is in the report: {:?}",
        report.permutations
    );
    assert!(
        report.undeclared.is_empty(),
        "undeclared defines: {:?}",
        report.undeclared
    );
    assert!(report.permutations.len() <= FORWARD_PERMUTATION_BUDGET);
    assert!(!report.over_budget);
}
