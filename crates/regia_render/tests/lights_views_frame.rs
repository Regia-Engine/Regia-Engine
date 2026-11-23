// SPDX-License-Identifier: MPL-2.0
//! More than one directional light, ambient volumes and the debug view modes in a real frame
//! (`specs/13` § 6, § 10, § 11; ADR-7211), on lavapipe.
//!
//! Every check compares two renders of the same scene at the same frame (a fresh renderer each),
//! one with the feature and one without (the control): ambient 0, no IBL and a fixed EV100, so the
//! only light is the one under test.

use regia_render::{
    DebugViewMode, DirectionalLightUniform, FrameAmbientVolume, FrameQuality, FrameRenderer, LabMaterial,
    LabMesh, LabScene, LabVertex, LightsViews, PostSettings, RenderDevice,
};

const W: u32 = 128;
const H: u32 = 96;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A quad in the XY plane centred on `(cx, cy)` at depth `z`, front face +Z.
fn quad(cx: f32, cy: f32, z: f32, half_w: f32, half_h: f32) -> LabMesh {
    let corners = [
        ([cx - half_w, cy - half_h, z], [0.0, 1.0]),
        ([cx + half_w, cy - half_h, z], [1.0, 1.0]),
        ([cx + half_w, cy + half_h, z], [1.0, 0.0]),
        ([cx - half_w, cy + half_h, z], [0.0, 0.0]),
    ];
    LabMesh {
        vertices: corners
            .iter()
            .map(|&(position, uv)| LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], uv))
            .collect(),
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    }
}

/// The sun of the scenes: travels into the screen and a little down, so it lights the +Z face.
fn sun_a(lux: f32) -> DirectionalLightUniform {
    DirectionalLightUniform::new([-0.3, -0.2, -1.0], [1.0, 1.0, 1.0], lux)
}

/// The second light: from the right, slightly warm.
fn light_b(lux: f32) -> DirectionalLightUniform {
    DirectionalLightUniform::new([-0.6, 0.1, -0.8], [1.0, 0.9, 0.8], lux)
}

/// One matte quad filling most of the view, camera on +Z. Ambient 0, no IBL, EV100 12.
fn scene(sun: DirectionalLightUniform, shadows: bool) -> LabScene {
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.05,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: shadows,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 12.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: sun,
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.45, 0.3, 1.0],
            roughness: 0.9,
            metallic: 0.0,
            ..LabMaterial::default()
        }],
        meshes: vec![quad(0.0, 0.0, 0.0, 2.0, 1.5)],
    }
}

fn render_with(scene: &LabScene, views: &LightsViews) -> Vec<u8> {
    let device = device();
    let frame = scene.as_frame_scene_with_quality(FrameQuality::LAB);
    let mut renderer = FrameRenderer::new();
    renderer.set_lights_views(views);
    renderer.render_offscreen(&device, &frame).expect("frame").pixels
}

fn render(scene: &LabScene) -> Vec<u8> {
    render_with(scene, &LightsViews::default())
}

/// Mean Rec. 709 luma of the pixel rectangle `[x0, x1) × [y0, y1)` given as fractions of the view.
fn luma(px: &[u8], x0: f32, x1: f32, y0: f32, y1: f32) -> f64 {
    let (x0, x1) = ((x0 * W as f32) as u32, (x1 * W as f32) as u32);
    let (y0, y1) = ((y0 * H as f32) as u32, (y1 * H as f32) as u32);
    let (mut sum, mut n) = (0.0, 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * W + x) * 4) as usize;
            sum += 0.2126 * f64::from(px[i]) + 0.7152 * f64::from(px[i + 1]) + 0.0722 * f64::from(px[i + 2]);
            n += 1.0;
        }
    }
    sum / n
}

fn centre(px: &[u8]) -> f64 {
    luma(px, 0.4, 0.6, 0.4, 0.6)
}

/// Mean absolute difference of the colour channels over the centre half of the view.
fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    let (mut sum, mut n) = (0.0, 0.0);
    for y in H / 4..3 * H / 4 {
        for x in W / 4..3 * W / 4 {
            let i = ((y * W + x) * 4) as usize;
            for c in 0..3 {
                sum += (f64::from(a[i + c]) - f64::from(b[i + c])).abs();
                n += 1.0;
            }
        }
    }
    sum / n
}

fn extras(lights: &[DirectionalLightUniform]) -> LightsViews {
    LightsViews {
        extra_directional: lights.to_vec(),
        ..LightsViews::default()
    }
}

#[test]
fn an_extra_directional_light_shades_like_the_sun_without_shadows() {
    // The same light as the frame's sun (shadows off) and as an extra light under a dark sun: the
    // extra light takes the sun's BRDF, so the two frames match.
    let as_sun = render(&scene(light_b(20_000.0), false));
    let as_extra = render_with(&scene(sun_a(0.0), false), &extras(&[light_b(20_000.0)]));
    let (sun_l, extra_l) = (centre(&as_sun), centre(&as_extra));
    eprintln!("light B as the sun {sun_l:.2}, as an extra light {extra_l:.2}");
    assert!(sun_l > 30.0, "light B must light the quad: {sun_l:.2}");
    assert!(
        (sun_l - extra_l).abs() <= 1.0,
        "an extra directional light must shade like the sun: extra {extra_l:.2} vs sun {sun_l:.2}"
    );
}

#[test]
fn a_second_directional_light_adds_its_lambert_term() {
    let control = render(&scene(sun_a(8_000.0), false));
    let both = render_with(&scene(sun_a(8_000.0), false), &extras(&[light_b(8_000.0)]));
    let (control_l, both_l) = (centre(&control), centre(&both));
    eprintln!("sun only {control_l:.2}, sun + second light {both_l:.2}");
    assert!(control_l > 10.0, "the sun must light the quad: {control_l:.2}");
    assert!(
        both_l > control_l + 10.0,
        "a second directional light must add its term: {both_l:.2} vs {control_l:.2}"
    );
    // Both lights shadowed by nothing: the sun's cascades on changes nothing about the extra term.
    let shadowed = render_with(&scene(sun_a(8_000.0), true), &extras(&[light_b(8_000.0)]));
    assert!(
        (centre(&shadowed) - both_l).abs() <= 1.5,
        "nothing casts on the quad: {:.2} vs {both_l:.2}",
        centre(&shadowed)
    );
}

#[test]
fn a_fourth_extra_light_is_refused() {
    // Spec 13: at most four directional lights, the sun and three more. A fourth extra (a fifth
    // light) never reaches the frame, however bright.
    let three = [light_b(2_000.0), light_b(2_000.0), light_b(2_000.0)];
    let mut four = three.to_vec();
    four.push(DirectionalLightUniform::new(
        [0.0, 0.0, -1.0],
        [1.0; 3],
        200_000.0,
    ));
    let base = scene(sun_a(2_000.0), false);
    let with_three = render_with(&base, &extras(&three));
    let with_four = render_with(&base, &extras(&four));
    let diff = mean_abs_diff(&with_three, &with_four);
    eprintln!("three extra lights vs four: mean diff {diff:.3}");
    assert!(
        diff <= 0.01,
        "the fifth directional light must be refused: diff {diff:.3}"
    );
    assert!(
        centre(&with_three) > centre(&render(&base)) + 5.0,
        "the three extra lights must shade"
    );
}

#[test]
fn an_ambient_volume_changes_the_ambient_term_inside_only() {
    // The quad spans x in [-2, 2]; the volume covers x < -0.5 (left of the screen centre).
    let base = scene(sun_a(4_000.0), false);
    let control = render(&base);
    let volume = LightsViews {
        ambient_volumes: vec![FrameAmbientVolume {
            center: [-2.0, 0.0, 0.0],
            half_extents: [1.5, 3.0, 1.0],
            irradiance: [12_000.0, 12_000.0, 12_000.0],
            blend_distance: 0.0,
            ..FrameAmbientVolume::default()
        }],
        ..LightsViews::default()
    };
    let with = render_with(&base, &volume);
    // Columns of the left and right quarters, rows of the middle half.
    let (left_c, left_v) = (
        luma(&control, 0.05, 0.2, 0.3, 0.7),
        luma(&with, 0.05, 0.2, 0.3, 0.7),
    );
    let (right_c, right_v) = (
        luma(&control, 0.8, 0.95, 0.3, 0.7),
        luma(&with, 0.8, 0.95, 0.3, 0.7),
    );
    eprintln!("left {left_c:.2} -> {left_v:.2}, right {right_c:.2} -> {right_v:.2}");
    assert!(right_c > 5.0, "the sun must light the right side: {right_c:.2}");
    assert!(
        left_v > left_c + 10.0,
        "inside the volume the ambient term must rise: {left_v:.2} vs {left_c:.2}"
    );
    assert!(
        (right_v - right_c).abs() <= 0.5,
        "outside the volume nothing changes: {right_v:.2} vs {right_c:.2}"
    );
}

/// The debug view modes the forward shader draws.
const SHADER_MODES: [DebugViewMode; 10] = [
    DebugViewMode::Unlit,
    DebugViewMode::Normals,
    DebugViewMode::BaseColor,
    DebugViewMode::Roughness,
    DebugViewMode::Metallic,
    DebugViewMode::AmbientOcclusion,
    DebugViewMode::LightComplexity,
    DebugViewMode::ShadowCascades,
    DebugViewMode::MotionVectors,
    DebugViewMode::MipLevel,
];

fn non_black(px: &[u8]) -> bool {
    let (mut lit, mut n) = (0usize, 0usize);
    for y in H / 4..3 * H / 4 {
        for x in W / 4..3 * W / 4 {
            let i = ((y * W + x) * 4) as usize;
            n += 1;
            if px[i].max(px[i + 1]).max(px[i + 2]) > 8 {
                lit += 1;
            }
        }
    }
    lit * 2 > n
}

#[test]
fn every_shader_view_mode_draws_a_non_black_image_that_differs_from_lit() {
    // A partly metallic surface, so the metallic view has something to show.
    let mut base = scene(sun_a(30_000.0), true);
    base.materials[0].metallic = 0.4;
    let lit = render(&base);
    assert!(non_black(&lit), "the lit control must be lit");
    for mode in SHADER_MODES {
        assert!(
            regia_render::DebugView::is_supported(mode),
            "{mode:?} must be supported"
        );
        let views = LightsViews {
            shading_view: mode,
            ..LightsViews::default()
        };
        let image = render_with(&base, &views);
        let diff = mean_abs_diff(&image, &lit);
        eprintln!(
            "{mode:?}: centre luma {:.2}, mean diff from lit {diff:.2}",
            centre(&image)
        );
        assert!(non_black(&image), "{mode:?} must draw a non-black image");
        assert!(diff > 3.0, "{mode:?} must differ from the lit frame: {diff:.2}");
    }
}

#[test]
fn the_overdraw_view_brightens_where_surfaces_overlap() {
    // Two quads overlapping in the middle third of the view, drawn with the overdraw material.
    let mut base = scene(sun_a(30_000.0), false);
    let lit = render(&base);
    base.materials = vec![regia_render::debug_shading::overdraw_material()];
    base.meshes = vec![quad(-0.6, 0.0, 0.0, 1.2, 1.2), quad(0.6, 0.0, -0.5, 1.2, 1.2)];
    let image = render(&base);
    let single = luma(&image, 0.2, 0.3, 0.4, 0.6);
    let overlap = luma(&image, 0.47, 0.53, 0.4, 0.6);
    eprintln!("overdraw: one layer {single:.2}, two layers {overlap:.2}");
    assert!(single > 8.0, "one layer must show: {single:.2}");
    assert!(
        overlap > single + 8.0,
        "two layers must be brighter than one: {overlap:.2} vs {single:.2}"
    );
    assert!(
        mean_abs_diff(&image, &lit) > 3.0,
        "the overdraw view must differ from lit"
    );
}

#[test]
fn the_lod_view_colors_each_level() {
    let base = scene(sun_a(30_000.0), false);
    let lit = render(&base);
    let mut colors = Vec::new();
    for level in 0..4 {
        let mut s = base.clone();
        s.materials = vec![regia_render::debug_shading::lod_color_material(level)];
        let image = render(&s);
        assert!(non_black(&image), "LOD {level} must draw a non-black image");
        assert!(
            mean_abs_diff(&image, &lit) > 3.0,
            "LOD {level} must differ from lit"
        );
        let i = (((H / 2) * W + W / 2) * 4) as usize;
        colors.push([image[i], image[i + 1], image[i + 2]]);
    }
    eprintln!("LOD colors {colors:?}");
    for (a, ca) in colors.iter().enumerate() {
        for (b, cb) in colors.iter().enumerate().skip(a + 1) {
            assert_ne!(ca, cb, "LOD {a} and {b} must differ");
        }
    }
}

#[test]
fn a_45_degree_ambient_volume_lights_its_own_diagonal_band() {
    // post05w4 item 2: a long thin box (6 m x 0.5 m in the quad's plane) turned 45 degrees about
    // the view axis lights a diagonal band. The world point (1, 1) is on the band's axis (1.41 m
    // along it) and must brighten; (1.5, 0) is inside the unturned box but 1.06 m off the turned
    // one and must not change.
    let base = scene(sun_a(4_000.0), false);
    let control = render(&base);
    let half = std::f32::consts::FRAC_PI_8;
    let volume = LightsViews {
        ambient_volumes: vec![FrameAmbientVolume {
            center: [0.0, 0.0, 0.0],
            half_extents: [3.0, 0.25, 1.0],
            irradiance: [12_000.0, 12_000.0, 12_000.0],
            blend_distance: 0.0,
            // 45 degrees about +Z (x, y, z, w).
            rotation: [0.0, 0.0, half.sin(), half.cos()],
        }],
        ..LightsViews::default()
    };
    let with = render_with(&base, &volume);
    // Camera at z = 3, fov_y 0.9: the view spans x in [-1.93, 1.93], y in [-1.45, 1.45] at z = 0.
    // (1, 1) is at u 0.76, v 0.15; (1.5, 0) is at u 0.89, v 0.5.
    let (on_c, on_v) = (
        luma(&control, 0.73, 0.79, 0.12, 0.19),
        luma(&with, 0.73, 0.79, 0.12, 0.19),
    );
    let (off_c, off_v) = (
        luma(&control, 0.86, 0.92, 0.45, 0.55),
        luma(&with, 0.86, 0.92, 0.45, 0.55),
    );
    eprintln!("on the band {on_c:.2} -> {on_v:.2}, off the band {off_c:.2} -> {off_v:.2}");
    assert!(off_c > 5.0, "the sun must light the quad: {off_c:.2}");
    assert!(
        on_v > on_c + 10.0,
        "on the turned band the ambient term must rise: {on_v:.2} vs {on_c:.2}"
    );
    assert!(
        (off_v - off_c).abs() <= 0.5,
        "inside the unturned box but off the turned one nothing changes: {off_v:.2} vs {off_c:.2}"
    );
}
