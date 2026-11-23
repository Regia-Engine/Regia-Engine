// SPDX-License-Identifier: MPL-2.0
//! The per-map 3D LUT (look-r2 round 3): an identity LUT leaves the frame (and so every
//! golden) unchanged; a tinted LUT changes it the way the LUT says.

use std::sync::Arc;

use regia_render::{
    ColorLut, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, box_mesh, translate_mesh, uv_sphere,
};

const W: u32 = 160;
const H: u32 = 90;

fn render(device: &RenderDevice, lut: Option<Arc<ColorLut>>) -> Vec<u8> {
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
    let lab = LabScene {
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
    };
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopLow));
    let mut renderer = FrameRenderer::new();
    renderer
        .render_offscreen(device, &lab.as_frame_scene_with_quality(quality))
        .expect("frame")
        .pixels
}

fn mean_rgb(pixels: &[u8]) -> [f32; 3] {
    let mut s = [0.0f32; 3];
    for p in pixels.as_chunks::<4>().0 {
        for k in 0..3 {
            s[k] += f32::from(p[k]);
        }
    }
    let n = (pixels.len() / 4) as f32;
    s.map(|v| v / n)
}

#[test]
fn identity_lut_is_invisible_and_a_tinted_lut_tints() {
    let device = RenderDevice::headless().expect("lavapipe");
    let plain = render(&device, None);
    let identity = render(&device, Some(Arc::new(ColorLut::identity(32))));
    let worst = plain
        .iter()
        .zip(&identity)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    eprintln!("identity LUT: worst byte difference {worst}");
    assert!(worst <= 1, "an identity LUT changed the frame by {worst} levels");

    // A warm-to-cool LUT: blue up, red down.
    let tint = ColorLut::from_fn(32, |c| [c[0] * 0.8, c[1], (c[2] * 1.25).min(1.0)]);
    let tinted = render(&device, Some(Arc::new(tint)));
    let (a, b) = (mean_rgb(&plain), mean_rgb(&tinted));
    eprintln!("mean rgb plain {a:?} tinted {b:?}");
    assert!(b[0] < a[0] - 5.0, "red must drop: {a:?} -> {b:?}");
    assert!(b[2] > a[2] + 3.0, "blue must rise: {a:?} -> {b:?}");
    assert!((b[1] - a[1]).abs() < 2.0, "green is untouched: {a:?} -> {b:?}");
}
