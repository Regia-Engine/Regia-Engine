// SPDX-License-Identifier: MPL-2.0
//! The many-light sampler's evaluation pass on real frames (ADR-2470): registered at
//! `after_opaque`, it records on every frame without failures, rebuilds nothing while the camera
//! and lights stay, draws a lit image whose average matches the brute-force view, and a frame
//! without it is unchanged. Lavapipe is the reference; no golden image is involved.

use std::sync::atomic::Ordering;

use regia_core::glam::Mat4;
use regia_render::restir_di::scene::scatter_lights;
use regia_render::restir_di::{
    ManyLightFeed, RESTIR_DI_PASS_NAME, RestirDebugView, RestirDiDebugPass, restir_di_counters,
};
use regia_render::{
    DirectionalLightUniform, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings, RenderDevice,
    box_mesh, frame_path_counters,
};

const W: u32 = 96;
const H: u32 = 54;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn block(centre: [f32; 3], half: [f32; 3]) -> LabMesh {
    let (vertices, indices) = box_mesh(half);
    LabMesh {
        vertices,
        indices,
        material: 0,
        model: Mat4::from_translation(centre.into()).to_cols_array_2d(),
        ..LabMesh::unskinned()
    }
}

/// A floor, a back wall and two boxes under a dim sun (the local lights come from the feed).
fn lab() -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 6.0, -14.0],
        target: [0.0, 0.5, 8.0],
        fov_y: 1.0,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 4.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 4.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.2, -1.0, 0.3], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.7, 0.7, 1.0],
            roughness: 0.8,
            ..LabMaterial::default()
        }],
        meshes: vec![
            block([0.0, -0.25, 12.0], [30.0, 0.25, 30.0]),
            block([0.0, 5.0, 30.25], [30.0, 5.0, 0.25]),
            block([-3.0, 1.0, 8.0], [1.0, 1.0, 1.0]),
            block([4.0, 1.5, 12.0], [1.5, 1.5, 1.5]),
        ],
    }
}

fn frame(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> Vec<u8> {
    renderer
        .render_offscreen(device, &lab.as_frame_scene())
        .expect("offscreen frame")
        .pixels
}

/// Mean of the RGB channels over the image, 0..255.
fn mean(pixels: &[u8]) -> f64 {
    let (texels, _) = pixels.as_chunks::<4>();
    texels
        .iter()
        .map(|p| (f64::from(p[0]) + f64::from(p[1]) + f64::from(p[2])) / 3.0)
        .sum::<f64>()
        / texels.len().max(1) as f64
}

fn lit(pixels: &[u8]) -> usize {
    let (texels, _) = pixels.as_chunks::<4>();
    texels.iter().filter(|p| p[0].max(p[1]).max(p[2]) > 8).count()
}

#[test]
fn the_evaluation_pass_draws_the_sampler_over_real_frames() {
    let device = device();
    let lab = lab();
    let mut renderer = FrameRenderer::new();
    let plain = frame(&mut renderer, &device, &lab);

    let feed = ManyLightFeed::default();
    feed.set_lights(scatter_lights(400, 11));
    feed.set_view(RestirDebugView::Reference);
    renderer
        .register_pass(Box::new(RestirDiDebugPass::new(feed.clone())))
        .expect("register");
    let c = restir_di_counters();
    let recorded = c.frames_recorded.load(Ordering::Relaxed);
    let uploads = c.light_uploads.load(Ordering::Relaxed);
    let builds = c.cluster_builds.load(Ordering::Relaxed);
    let reference = frame(&mut renderer, &device, &lab);
    assert_ne!(reference, plain, "the pass draws over the view");
    assert!(
        lit(&reference) > (W * H / 2) as usize,
        "the lights reach most of the view: {}",
        lit(&reference)
    );

    feed.set_view(RestirDebugView::Estimate);
    let mut estimate = 0.0;
    const FRAMES: u32 = 12;
    let mut last = Vec::new();
    for _ in 0..FRAMES {
        last = frame(&mut renderer, &device, &lab);
        estimate += mean(&last) / f64::from(FRAMES);
    }
    let r = mean(&reference);
    println!(
        "reference mean {r:.2}, estimate mean {estimate:.2} over {FRAMES} frames, lit {}",
        lit(&last)
    );
    // The display is tonemapped (concave), so the noisy estimate averages a little below the
    // reference; a black, garbage or double-counted image is far outside this band.
    assert!(
        estimate > 0.6 * r && estimate < 1.15 * r,
        "estimate {estimate} vs reference {r}"
    );

    let report = renderer.graph_report();
    let Some(info) = report.extensions.iter().find(|p| p.name == RESTIR_DI_PASS_NAME) else {
        panic!("the pass is in the graph");
    };
    assert_eq!(info.failures, 0, "{:?}", info.last_error);
    assert_eq!(info.runs, u64::from(FRAMES) + 1);
    assert_eq!(
        c.frames_recorded.load(Ordering::Relaxed) - recorded,
        u64::from(FRAMES) + 1
    );
    assert_eq!(
        c.light_uploads.load(Ordering::Relaxed) - uploads,
        1,
        "one light set, one upload"
    );
    assert_eq!(
        c.cluster_builds.load(Ordering::Relaxed) - builds,
        1,
        "a still camera builds its lists once"
    );
    assert_eq!(c.last_width.load(Ordering::Relaxed), u64::from(W));

    // Steady frames create nothing.
    let before = frame_path_counters();
    for _ in 0..4 {
        frame(&mut renderer, &device, &lab);
    }
    let after = frame_path_counters().since(before);
    assert_eq!(
        (after.pipelines, after.bind_groups, after.buffers, after.textures),
        (0, 0, 0, 0),
        "{after:?}"
    );

    // Removed, the frame is the plain frame again.
    assert!(renderer.remove_pass(RESTIR_DI_PASS_NAME).is_some());
    let back = frame(&mut renderer, &device, &lab);
    assert!(
        (mean(&back) - mean(&plain)).abs() < 0.5,
        "{} vs {}",
        mean(&back),
        mean(&plain)
    );
}
