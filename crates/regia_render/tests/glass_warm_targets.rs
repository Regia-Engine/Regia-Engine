// SPDX-License-Identifier: MPL-2.0
//! The game-frame warm takes the pooled glass targets (ADR-2018): the refraction copy and the
//! glass front layer are transients of the view's pool, created through the tracked device, so
//! a level with refracting glass must not create them on its first frame of play.
//!
//! - Warmed at the window size (`warm_extracted_at` with a window, what `GameFrameWarm` runs),
//!   the first refracting frames at that size create no transient.
//! - Warmed without the window frame, the first refracting frame at the play size does create
//!   them (the test sees the case it guards).

use std::sync::Arc;

use regia_render::pipeline_warm::canonical_frame;
use regia_render::runtime::ExtractedMesh;
use regia_render::{ExtractedFrame, FrameRenderer, LabMaterial, RenderDevice};

const W: u32 = 160;
const H: u32 = 96;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// The canonical warm frame with a refracting pane (a volume thickness) above its floor, and
/// screen-space refraction on.
fn frame_with_refracting_pane() -> ExtractedFrame {
    let mut frame = canonical_frame();
    let base = frame.meshes[0].clone();
    let entity = regia_ecs::prelude::Entity::from_raw_u32(7).expect("entity index");
    frame.meshes.push(ExtractedMesh {
        entity,
        material: Arc::new(LabMaterial {
            base_color: [0.8, 0.9, 1.0, 0.2],
            transmission: 1.0,
            transmission_thickness: 0.05,
            roughness: 0.05,
            ..LabMaterial::default()
        }),
        model: regia_core::math::Mat4::from_translation(regia_core::math::Vec3::new(0.0, 0.5, 0.0))
            .to_cols_array_2d(),
        ..base
    });
    frame.quality.screen_space_refraction = true;
    frame.quality.msaa_samples = 1;
    frame
}

/// Draws the first view at the play size and returns the pool's create count and the textures
/// the frame took.
fn play_frame(device: &RenderDevice, renderer: &mut FrameRenderer, frame: &ExtractedFrame) -> (u64, usize) {
    let (view_index, _) = frame.window_views()[0];
    let scene = frame.frame_scene(view_index, W, H);
    renderer.render_offscreen(device, &scene).expect("frame");
    let pool = renderer.graph_report().graph.transients;
    (pool.created, pool.in_use)
}

#[test]
fn the_window_warm_takes_the_glass_targets_so_play_creates_none() {
    let device = device();
    let frame = frame_with_refracting_pane();
    let mut renderer = FrameRenderer::new();
    renderer
        .warm_extracted_at(
            &device,
            &frame,
            wgpu::TextureFormat::Rgba8Unorm,
            true,
            Some((W, H)),
        )
        .expect("warm");
    let warmed = renderer.graph_report().graph.transients.created;
    for index in 0..3 {
        let (created, in_use) = play_frame(&device, &mut renderer, &frame);
        // HDR target, refraction copy, front layer depth and transmittance (SSAO adds two).
        assert!(
            in_use >= 4,
            "frame {index} did not refract: {in_use} textures taken"
        );
        assert_eq!(
            created, warmed,
            "frame {index} created a pooled target after the warm"
        );
    }

    // The same level warmed without the window frame: the play size's glass targets are new.
    let mut cold = FrameRenderer::new();
    cold.warm_extracted(&device, &frame, wgpu::TextureFormat::Rgba8Unorm, true)
        .expect("warm");
    let before = cold.graph_report().graph.transients.created;
    let (created, in_use) = play_frame(&device, &mut cold, &frame);
    assert!(
        in_use >= 4,
        "the control frame did not refract: {in_use} textures taken"
    );
    assert!(
        created >= before + 4,
        "the control frame should create its targets at the play size: {before} -> {created}"
    );
}
