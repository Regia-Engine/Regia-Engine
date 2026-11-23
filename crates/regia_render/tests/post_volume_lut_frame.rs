// SPDX-License-Identifier: MPL-2.0
//! Post-volume LUT blending on the GPU (ADR-2323, spec 30 § 10.2): a view's LUT that changes
//! from frame to frame (a camera walking through a volume's band) is uploaded into the post pass's
//! existing 3D texture when the edge is the same, and the frame shows the new table, not the old
//! one. No goldens: frames are compared with each other (lavapipe or any adapter).

use std::sync::Arc;

use regia_render::post_volume_luts::{LutContribution, blend_luts};
use regia_render::{
    ColorLut, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh, translate_mesh, uv_sphere,
};

const W: u32 = 160;
const H: u32 = 90;

fn scene(lut: Option<Arc<ColorLut>>) -> LabScene {
    let post = PostSettings {
        lut,
        exposure_ev: 14.0,
        ..PostSettings::golden_deterministic()
    };
    let (sv, si) = uv_sphere(1.0, 32, 16);
    let (bv, bi) = box_mesh([4.0, 0.1, 4.0]);
    let mat = |c: [f32; 3], r: f32| LabMaterial {
        base_color: [c[0], c[1], c[2], 1.0],
        roughness: r,
        ..LabMaterial::default()
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 1.5, 4.0],
        target: [0.0, 0.8, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([0.4, -0.8, -0.3], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![mat([0.7, 0.25, 0.1], 0.5), mat([0.5, 0.5, 0.5], 0.8)],
        meshes: vec![
            LabMesh {
                vertices: translate_mesh(&sv, [0.0, 1.0, 0.0]),
                indices: si,
                material: 0,
                ..LabMesh::unskinned()
            },
            LabMesh {
                vertices: translate_mesh(&bv, [0.0, -0.1, 0.0]),
                indices: bi,
                material: 1,
                ..LabMesh::unskinned()
            },
        ],
    }
}

fn frame(renderer: &mut FrameRenderer, device: &RenderDevice, lut: &Arc<ColorLut>) -> Vec<u8> {
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopLow));
    renderer
        .render_offscreen(
            device,
            &scene(Some(lut.clone())).as_frame_scene_with_quality(quality),
        )
        .expect("frame")
        .pixels
}

fn mean_red(pixels: &[u8]) -> f32 {
    let (chunks, _) = pixels.as_chunks::<4>();
    chunks.iter().map(|p| f32::from(p[0])).sum::<f32>() / chunks.len() as f32
}

#[test]
fn a_changed_lut_of_the_same_edge_reaches_the_next_frame() {
    let device = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let warm = Arc::new(ColorLut::from_fn(17, |c| {
        [(c[0] * 1.3).min(1.0), c[1], c[2] * 0.7]
    }));
    let cool = Arc::new(ColorLut::from_fn(17, |c| {
        [c[0] * 0.6, c[1], (c[2] * 1.3).min(1.0)]
    }));

    // One renderer sees the warm table, then the cool one (the texture is reused); another sees
    // the cool table twice. Their second frames must be the same bytes.
    let mut reused = FrameRenderer::new();
    let warm_frame = frame(&mut reused, &device, &warm);
    let after_switch = frame(&mut reused, &device, &cool);
    let mut fresh = FrameRenderer::new();
    let _ = frame(&mut fresh, &device, &cool);
    let cool_frame = frame(&mut fresh, &device, &cool);
    assert_ne!(warm_frame, cool_frame, "the two tables must look different");
    let worst = after_switch
        .iter()
        .zip(&cool_frame)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert_eq!(
        worst, 0,
        "the reused LUT texture still showed the old table ({worst} levels)"
    );

    // A half-way blend (a camera in the middle of a band) lands between the two looks.
    let half = blend_luts(
        Some(&warm),
        &mut [LutContribution {
            weight: 0.5,
            priority: 0,
            key: 1,
            lut: cool.clone(),
        }],
    )
    .expect("blend");
    let half_frame = frame(&mut reused, &device, &half);
    let (w, h, c) = (
        mean_red(&warm_frame),
        mean_red(&half_frame),
        mean_red(&cool_frame),
    );
    eprintln!("mean red warm {w} half {h} cool {c}");
    assert!(
        w > h + 2.0 && h > c + 2.0,
        "the blend is between the looks: {w} {h} {c}"
    );
}
