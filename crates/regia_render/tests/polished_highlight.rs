// SPDX-License-Identifier: MPL-2.0
//! The GGX α floor (`pbr_lighting.wgsl` `MIN_ALPHA` = perceptual 0.045 squared).
//!
//! A black polished dielectric (perceptual roughness 0.03, DO27's smoked glass) under the sun,
//! seen 3° (half-vector) off the sun's mirror direction: the forward pixel is the GGX lobe at
//! α = 0.002025 within a factor of two. The old floor (α ≥ 0.045, perceptual 0.21) shaded it
//! ~160× brighter there: the highlight smeared across the whole pane.

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset,
};

const W: u32 = 96;
const H: u32 = 96;
const SUN_LUX: f32 = 100_000.0;

fn d_ggx(no_h: f32, a: f32) -> f32 {
    let a2 = a * a;
    let f = (no_h * a2 - no_h) * no_h + 1.0;
    a2 / (std::f32::consts::PI * f * f)
}

fn v_smith(no_v: f32, no_l: f32, a: f32) -> f32 {
    let a2 = a * a;
    let gv = no_l * (no_v * no_v * (1.0 - a2) + a2).sqrt();
    let gl = no_v * (no_l * no_l * (1.0 - a2) + a2).sqrt();
    0.5 / (gv + gl)
}

/// Single-scatter GGX radiance of a black dielectric (F0 0.04) for the sun at `l`.
fn analytic(n: Vec3, v: Vec3, l: Vec3, a: f32) -> f32 {
    let h = (v + l).normalize();
    let (no_v, no_l, no_h, lo_h) = (n.dot(v), n.dot(l), n.dot(h), l.dot(h));
    let f = 0.04 + 0.96 * (1.0 - lo_h).powi(5);
    d_ggx(no_h, a) * v_smith(no_v, no_l, a) * f * SUN_LUX * no_l
}

#[test]
fn a_polished_surface_keeps_its_highlight_tight() {
    let n = Vec3::Y;
    let eye = Vec3::new(0.0, 2.0, 2.0);
    let v = eye.normalize();
    // Half vector 3° from the normal, in the plane of incidence; the sun is v mirrored about it.
    let off = 3f32.to_radians();
    let h = Vec3::new(0.0, off.cos(), -off.sin());
    let l = (2.0 * v.dot(h) * h - v).normalize();
    assert!(l.y > 0.2, "sun below the horizon: {l:?}");
    let want = analytic(n, v, l, 0.002_025);
    let old = analytic(n, v, l, 0.045);

    let floor = LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        roughness: 0.03,
        ..LabMaterial::default()
    };
    let (vertices, indices) = regia_render::box_mesh([10.0, 0.01, 10.0]);
    let post = PostSettings {
        exposure_ev: 14.0,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: W,
        height: H,
        eye: eye.to_array(),
        target: [0.0, 0.0, 0.0],
        fov_y: 0.3,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post.clone()),
        directional: DirectionalLightUniform::new((-l).to_array(), [1.0, 1.0, 1.0], SUN_LUX),
        local_lights: Vec::new(),
        materials: vec![floor],
        meshes: vec![LabMesh {
            vertices,
            indices,
            material: 0,
            model: Mat4::from_translation(Vec3::new(0.0, -0.01, 0.0)).to_cols_array_2d(),
            ..LabMesh::unskinned()
        }],
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.taau = false;
    let mut frame = scene.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let mut renderer = FrameRenderer::new();
    renderer.request_hdr_capture();
    renderer.render_offscreen(&device, &frame).expect("frame");
    let hdr = renderer
        .take_hdr_capture(&device)
        .expect("hdr readback")
        .expect("the frame ran the post stack");
    let (cx, cy) = (hdr.width / 2, hdr.height / 2);
    let mut sum = 0.0;
    for y in cy - 1..cy + 1 {
        for x in cx - 1..cx + 1 {
            let [r, g, b] = hdr.rgb[(y * hdr.width + x) as usize];
            sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
        }
    }
    let got = sum / 4.0;
    eprintln!("center {got} cd/m², GGX at alpha 0.002025: {want}, at the old floor 0.045: {old}");
    assert!(
        old > 50.0 * want,
        "the test geometry no longer separates the floors"
    );
    assert!(
        got > 0.5 * want && got < 2.0 * want,
        "{got} vs {want} (old floor {old})"
    );
}
