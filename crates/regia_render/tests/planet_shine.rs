// SPDX-License-Identifier: MPL-2.0
//! Planet-shine (look-r2 round 6): a level sky body throws a shadowless directional fill
//! (Jupiter-shine on Europa, DO27's `fill_lux` 1600). A face in the sun's shadow that
//! faces the body brightens by exactly the Lambert term `albedo / π · E_fill`; a sun-lit
//! face turned away from the body does not change.

use regia_core::glam::{Mat4, Vec3, Vec4};
use regia_render::{
    AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SkyBody, SsaoMode,
    box_mesh, exposure_from_ev100, inverse_tonemap_agx,
};

const W: u32 = 320;
const H: u32 = 180;
const EV: f32 = 11.0;
const ALBEDO: f32 = 0.5;
const EYE: [f32; 3] = [5.0, 1.5, 5.0];
const FOV_Y: f32 = 0.8;

fn render(device: &RenderDevice, fill_lux: f32) -> Vec<u8> {
    // Sun from +X, a little behind (−Z): the +X face is lit, the +Z face is in shadow.
    let to_sun = Vec3::new(1.0, 0.6, -0.3).normalize();
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: EV,
        bloom: false,
        fxaa: false,
        taa: false,
        contrast: 1.0,
        saturation: 1.0,
        ..PostSettings::default()
    };
    let (vertices, indices) = box_mesh([1.0, 1.0, 1.0]);
    let lab = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: [0.0, 0.0, 0.0],
        fov_y: FOV_Y,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new((-to_sun).to_array(), [1.0; 3], 4_400.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [ALBEDO, ALBEDO, ALBEDO, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices,
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.ssao = SsaoMode::Off;
    quality.taau = false;
    quality.screen_space_gi = false;
    quality.ssr = false;
    let mut renderer = FrameRenderer::new();
    // The body straight along +Z (behind the camera), grey so the fill is neutral.
    let outdoor = OutdoorParams {
        planet: regia_render::AtmosphereParameters::from_preset(AtmospherePreset::Airless),
        sky_body: Some(SkyBody {
            direction: [0.0, 0.0, 1.0],
            tint: [0.55, 0.55, 0.55],
            fill_lux: Some(fill_lux),
            ..SkyBody::jupiter_from_europa()
        }),
        ..OutdoorParams::default()
    };
    renderer.set_outdoor(outdoor);
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut pixels = Vec::new();
    for _ in 0..3 {
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    pixels
}

/// Pixel of a world point.
fn project(p: Vec3) -> (u32, u32) {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(Vec3::from_array(EYE), Vec3::ZERO, Vec3::Y);
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(FOV_Y, W as f32 / H as f32, 0.1, 100.0);
    let clip: Vec4 = (proj * view as Mat4) * p.extend(1.0);
    let ndc = clip.truncate() / clip.w;
    (
        ((ndc.x * 0.5 + 0.5) * W as f32) as u32,
        ((0.5 - ndc.y * 0.5) * H as f32) as u32,
    )
}

/// Scene luminance (cd/m²) around a pixel: the display value through the inverse AgX and
/// the exposure.
fn luminance(pixels: &[u8], at: (u32, u32)) -> f32 {
    let mut sum = [0.0f32; 3];
    let mut n = 0.0;
    for dy in 0..5 {
        for dx in 0..5 {
            let (x, y) = (at.0 + dx - 2, at.1 + dy - 2);
            let i = ((y * W + x) * 4) as usize;
            for c in 0..3 {
                sum[c] += f32::from(pixels[i + c]) / 255.0;
            }
            n += 1.0;
        }
    }
    let rgb = inverse_tonemap_agx(sum.map(|v| v / n));
    let e = exposure_from_ev100(EV);
    (0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]) / e
}

#[test]
fn planet_shine_lifts_the_shadow_facing_the_body_and_leaves_the_sunlit_side() {
    let device = RenderDevice::headless().expect("lavapipe");
    let off = render(&device, 0.0);
    let on = render(&device, 1_600.0);
    let shadowed = project(Vec3::new(0.0, 0.0, 1.001));
    let lit = project(Vec3::new(1.001, 0.0, 0.0));
    let (s0, s1) = (luminance(&off, shadowed), luminance(&on, shadowed));
    let (l0, l1) = (luminance(&off, lit), luminance(&on, lit));
    // Lambert diffuse of a 0.5 albedo face square to a 1600 lx fill: 0.5 / π · 1600.
    let expected = ALBEDO / std::f32::consts::PI * 1_600.0;
    eprintln!(
        "shadowed face {s0:.1} → {s1:.1} cd/m² (+{:.1}, expected +{expected:.1}); lit face {l0:.1} → {l1:.1}",
        s1 - s0
    );
    assert!(
        ((s1 - s0) / expected - 1.0).abs() < 0.12,
        "the shadowed face rose by {:.1} cd/m², expected {expected:.1}",
        s1 - s0
    );
    assert!(
        (l1 / l0 - 1.0).abs() < 0.02,
        "the sun-lit face turned away from the body changed: {l0:.1} → {l1:.1}"
    );
}
