// SPDX-License-Identifier: MPL-2.0
//! CPU masked occlusion culling of the forward draw list (ADR-10072,
//! `FrameRenderer::set_masked_occlusion`, `RenderProfile::masked_occlusion`).
//!
//! - Counted: a wall in front of 100 boxes culls them (Desktop Low's column is on); Desktop
//!   High's column is off, and with it nothing is culled.
//! - The culled frame is byte-identical to the unculled frame (lavapipe, fixed scene and camera):
//!   culling never changes the image, only skips draws the depth test rejects.
//! - A camera moving over 16 fixed steps past the wall's edge never culls a visible box: each
//!   step's frame equals the unculled renderer's frame of the same step.
//! - A single-sided wall seen from behind (the GPU culls its faces) hides nothing, and a
//!   blended wall is never an occluder.
//!
//! The image tests need a headless device (lavapipe).

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    MaskedCullSkip, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh,
};

const W: u32 = 160;
const H: u32 = 96;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn translated(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [x, y, z, 1.0],
    ]
}

/// A quad in the XY plane at `z`, `half_w` by `half_h`, front face toward +Z when `toward_z`.
fn wall(half_w: f32, half_h: f32, z: f32, toward_z: bool, material: usize) -> LabMesh {
    let corners = [
        [-half_w, -half_h, z],
        [half_w, -half_h, z],
        [half_w, half_h, z],
        [-half_w, half_h, z],
    ];
    let normal = if toward_z {
        [0.0, 0.0, 1.0]
    } else {
        [0.0, 0.0, -1.0]
    };
    LabMesh {
        vertices: corners
            .iter()
            .map(|&p| LabVertex::from_pos_normal_uv(p, normal, [0.0, 0.0]))
            .collect(),
        indices: if toward_z {
            vec![0, 1, 2, 0, 2, 3]
        } else {
            vec![0, 2, 1, 0, 3, 2]
        },
        material,
        ..LabMesh::unskinned()
    }
}

/// `n` × `n` boxes of half-size 0.3 m on a grid in the XY plane at depth `z`, spaced 1 m.
fn boxes(n: usize, z: f32, material: usize) -> Vec<LabMesh> {
    let (vertices, indices) = box_mesh([0.3, 0.3, 0.3]);
    let mut out = Vec::new();
    let offset = (n as f32 - 1.0) * 0.5;
    for i in 0..n {
        for j in 0..n {
            out.push(LabMesh {
                vertices: vertices.clone(),
                indices: indices.clone(),
                material,
                model: translated(i as f32 - offset, j as f32 - offset, z),
                ..LabMesh::unskinned()
            });
        }
    }
    out
}

/// A wall at z = 0 (14 m × 10 m, front toward the camera unless `wall_toward_camera` is false)
/// with 100 boxes behind it at z = -6, and 9 boxes in front of it at z = 3 that stay visible.
/// `blend_wall` gives the wall a blended material.
fn scene(eye: [f32; 3], target: [f32; 3], wall_toward_camera: bool, blend_wall: bool) -> LabScene {
    let mut meshes = vec![wall(7.0, 5.0, 0.0, wall_toward_camera, 0)];
    meshes.extend(boxes(10, -6.0, 1));
    meshes.extend(boxes(3, 3.0, 2));
    LabScene {
        width: W,
        height: H,
        eye,
        target,
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 3000.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([-0.4, -0.8, -0.45], [1.0, 1.0, 1.0], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![
            LabMaterial {
                base_color: [0.6, 0.6, 0.62, if blend_wall { 0.5 } else { 1.0 }],
                roughness: 0.8,
                alpha_mode: if blend_wall {
                    regia_render::ALPHA_BLEND
                } else {
                    regia_render::ALPHA_OPAQUE
                },
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.8, 0.3, 0.2, 1.0],
                roughness: 0.5,
                ..LabMaterial::default()
            },
            LabMaterial {
                base_color: [0.2, 0.5, 0.8, 1.0],
                roughness: 0.5,
                ..LabMaterial::default()
            },
        ],
        meshes,
    }
}

fn low_quality() -> FrameQuality {
    FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopLow))
}

/// Renders `frames` frames of `scene` with the switch and returns the last frame's pixels and
/// the renderer.
fn render(device: &RenderDevice, scene: &LabScene, masked: bool, frames: usize) -> (Vec<u8>, FrameRenderer) {
    let mut renderer = FrameRenderer::new();
    renderer.set_masked_occlusion(masked);
    let frame = scene.as_frame_scene_with_quality(low_quality());
    let mut pixels = Vec::new();
    for _ in 0..frames {
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    (pixels, renderer)
}

fn differing_pixels(a: &[u8], b: &[u8]) -> usize {
    assert_eq!(a.len(), b.len());
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(x, y)| x != y)
        .count()
}

#[test]
fn the_column_is_on_for_low_and_the_deck_and_off_for_high() {
    let column = |preset| RenderProfile::from_preset(preset).masked_occlusion;
    assert!(
        column(RenderProfilePreset::DesktopLow),
        "Desktop Low culls on the CPU"
    );
    assert!(column(RenderProfilePreset::Deck), "the Deck culls on the CPU");
    assert!(
        !column(RenderProfilePreset::DesktopHigh),
        "Desktop High has the GPU Hi-Z path"
    );
    assert!(!column(RenderProfilePreset::DesktopUltra));
    assert!(!column(RenderProfilePreset::MobileLow), "not measured on mobile");
    assert!(!column(RenderProfilePreset::MobileHigh), "not measured on mobile");
}

#[test]
fn a_wall_in_front_of_100_boxes_culls_them_on_low_and_nothing_on_high() {
    let device = device();
    let scene = scene([0.0, 0.0, 12.0], [0.0, 0.0, 0.0], true, false);
    // Desktop Low's column: on.
    let low = RenderProfile::from_preset(RenderProfilePreset::DesktopLow).masked_occlusion;
    let (_, renderer) = render(&device, &scene, low, 1);
    let report = renderer.masked_occlusion_report();
    eprintln!("low: {report:?}");
    assert!(report.active, "the cull ran: {report:?}");
    // The wall, and the boxes in front of it (large enough on screen to be picked too).
    assert!(report.occluders >= 1, "the wall is an occluder: {report:?}");
    assert!(
        report.culled >= 90,
        "the wall hides the boxes behind it (100, a few at the wall's silhouette may stay): {report:?}"
    );
    assert!(
        report.culled <= 100,
        "the 9 boxes in front of the wall (and the wall) are never culled: {report:?}"
    );
    // Desktop High's column: off, nothing culled, the report says why.
    let high = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh).masked_occlusion;
    let (_, renderer) = render(&device, &scene, high, 1);
    let report = renderer.masked_occlusion_report();
    eprintln!("high: {report:?}");
    assert!(!report.active);
    assert_eq!(report.culled, 0);
    assert_eq!(report.tested, 0);
    assert_eq!(report.skipped, Some(MaskedCullSkip::Off));
}

#[test]
fn the_culled_frame_is_byte_identical_to_the_unculled_frame() {
    let device = device();
    let scene = scene([1.5, 1.0, 12.0], [0.0, 0.0, 0.0], true, false);
    let (on, renderer) = render(&device, &scene, true, 3);
    let report = renderer.masked_occlusion_report();
    assert!(report.culled > 0, "the culled frame culls: {report:?}");
    let (off, renderer) = render(&device, &scene, false, 3);
    assert_eq!(renderer.masked_occlusion_report().culled, 0);
    let diff = differing_pixels(&on, &off);
    assert_eq!(diff, 0, "culling changed {diff} pixels ({report:?})");
}

#[test]
fn a_moving_camera_over_16_steps_never_culls_a_visible_box() {
    let device = device();
    let mut on = FrameRenderer::new();
    on.set_masked_occlusion(true);
    let mut off = FrameRenderer::new();
    let mut total_culled = 0u32;
    for step in 0..16 {
        // Strafe from in front of the wall to past its right edge while closing in, so the boxes
        // behind it come into view step by step.
        let t = step as f32 / 15.0;
        let eye = [-2.0 + 12.0 * t, 0.5 + t, 13.0 - 5.0 * t];
        let target = [eye[0] - 1.5 * t, 0.0, -3.0];
        let scene = scene(eye, target, true, false);
        let frame = scene.as_frame_scene_with_quality(low_quality());
        let a = on.render_offscreen(&device, &frame).expect("frame").pixels;
        let b = off.render_offscreen(&device, &frame).expect("frame").pixels;
        let report = on.masked_occlusion_report();
        total_culled += report.culled;
        let diff = differing_pixels(&a, &b);
        eprintln!(
            "step {step}: eye {eye:?}, culled {} of {}, {diff} pixels differ",
            report.culled, report.tested
        );
        assert_eq!(diff, 0, "step {step}: culling changed {diff} pixels ({report:?})");
    }
    assert!(total_culled > 0, "the path culls something");
}

#[test]
fn a_back_facing_wall_and_a_blended_wall_hide_nothing() {
    let device = device();
    // The camera behind a single-sided wall: the GPU culls its faces, so the boxes show through.
    let behind = scene([0.0, 0.0, 12.0], [0.0, 0.0, 0.0], false, false);
    let (on, renderer) = render(&device, &behind, true, 1);
    let report = renderer.masked_occlusion_report();
    eprintln!("back-facing wall: {report:?}");
    assert_eq!(
        report.culled, 0,
        "a wall the GPU does not draw hides nothing: {report:?}"
    );
    let (off, _) = render(&device, &behind, false, 1);
    assert_eq!(differing_pixels(&on, &off), 0);
    // A blended wall is not an occluder.
    let blended = scene([0.0, 0.0, 12.0], [0.0, 0.0, 0.0], true, true);
    let (on, renderer) = render(&device, &blended, true, 1);
    let report = renderer.masked_occlusion_report();
    eprintln!("blended wall: {report:?}");
    assert_eq!(report.culled, 0, "a blended wall hides nothing: {report:?}");
    let (off, _) = render(&device, &blended, false, 1);
    assert_eq!(differing_pixels(&on, &off), 0);
}
