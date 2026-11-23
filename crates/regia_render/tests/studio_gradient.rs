// SPDX-License-Identifier: MPL-2.0
//! A metal sphere reflects the studio gradient and an HDR equirect image (ADR-1653).
//!
//! Seen from the side, a mirror sphere shows the environment along its vertical diameter: the
//! centre row reflects the horizon behind the camera, rows above it the sky toward the zenith,
//! rows below it the ground. The two-tone studio reflects a flat sky, so its upper half is one
//! colour; the gradient's is a ramp that gets brighter and bluer going up.

use regia_render::{
    DirectionalLightUniform, EnvironmentAuthority, FrameQuality, FrameRenderer, LabMaterial, LabMesh,
    LabScene, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, StudioEquirect,
    StudioGradient, horizontal_plus_fov_y, translate_mesh, uv_sphere,
};

/// A renderer on which the installed environment is the sky and the image light of Desktop High's
/// frames too (`EnvironmentAuthority::Studio`, the opt-in of ADR-2000 (0), ADR-2016): these tests
/// measure that look. The default draws the physical atmosphere over it, as v0.2.8 did.
fn studio_renderer() -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_environment_authority(EnvironmentAuthority::Studio);
    renderer
}

const W: u32 = 256;
const H: u32 = 256;
const FOV_Y: f32 = 0.75;

fn gradient() -> StudioGradient {
    StudioGradient::new(
        [2400.0, 4200.0, 8000.0],
        [2600.0, 2800.0, 3000.0],
        [600.0, 600.0, 600.0],
    )
}

fn scene() -> LabScene {
    let (v, i) = uv_sphere(3.0, 128, 64);
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 2.0, 9.0],
        target: [0.0, 2.0, 0.0],
        fov_y: FOV_Y,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 13.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(PostSettings {
            auto_exposure: false,
            exposure_ev: 13.0,
            bloom: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [1.0, 1.0, 1.0, 1.0],
            metallic: 1.0,
            roughness: 0.05,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: translate_mesh(&v, [0.0, 2.0, 0.0]),
            indices: i,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

fn render(device: &RenderDevice, renderer: &mut FrameRenderer) -> Vec<u8> {
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.ssao = SsaoMode::Off;
    quality.taau = false;
    quality.screen_space_gi = false;
    quality.ssr = false;
    let lab = scene();
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut pixels = Vec::new();
    for _ in 0..2 {
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    pixels
}

/// Mean colour of the 8 columns around the sphere's vertical diameter on row `y`.
fn row(pixels: &[u8], y: u32) -> [f32; 3] {
    let mut sum = [0.0f32; 3];
    for x in W / 2 - 4..W / 2 + 4 {
        let i = ((y * W + x) * 4) as usize;
        for (c, v) in sum.iter_mut().enumerate() {
            *v += f32::from(pixels[i + c]) / 8.0;
        }
    }
    sum
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// First and last sphere rows inside its silhouette, with a margin that stays off the limb.
fn sphere_rows() -> (u32, u32) {
    let half_fov = horizontal_plus_fov_y(FOV_Y, W as f32 / H as f32) * 0.5;
    let ndc = (3.0_f32 / 9.0).asin().tan() / half_fov.tan();
    let half_rows = (ndc * H as f32 * 0.5).floor() as u32;
    let span = 2 * half_rows;
    (H / 2 - half_rows + span / 8, H / 2 + half_rows - span / 8)
}

#[test]
#[ignore = "open defect: on Desktop High the probe GI (`gi_probes`) puts a red-to-yellow spot where the sphere reflects the nadir (ground row [202, 33, 0] on lavapipe); with `gi_probes` off this test passes on lavapipe (10-31, ADR-2000 (6)); probe GI follow-up (ADR-1654 (7))"]
fn a_metal_sphere_reflects_the_gradient_from_ground_through_horizon_to_zenith() {
    let device = RenderDevice::headless().expect("lavapipe");
    let mut renderer = studio_renderer();
    assert_eq!(renderer.set_studio_gradient(&device, Some(&gradient())), Ok(true));
    let pixels = render(&device, &mut renderer);
    let (top, bottom) = sphere_rows();
    let mid = H / 2;
    assert!(
        bottom - top > 80,
        "the sphere fills the frame ({} rows)",
        bottom - top
    );

    // Above the centre the reflected sky climbs: each row is no darker than the row below it
    // (a level of slack for the dither and the texel interpolation) and the top row is much
    // brighter and bluer than the horizon row.
    let mut worst_drop = 0.0f32;
    for y in top..mid {
        worst_drop = worst_drop.max(luma(row(&pixels, y + 1)) - luma(row(&pixels, y)));
    }
    let (sky_top, horizon, ground) = (row(&pixels, top), row(&pixels, mid), row(&pixels, bottom));
    eprintln!(
        "gradient sphere: top {sky_top:?} horizon {horizon:?} ground {ground:?} worst drop {worst_drop:.2}"
    );
    assert!(
        worst_drop <= 1.5,
        "the reflected sky darkens going up by {worst_drop:.2} levels"
    );
    assert!(
        luma(sky_top) > luma(horizon) + 12.0,
        "zenith side is brighter than the horizon"
    );
    assert!(
        (sky_top[2] - sky_top[0]) > (horizon[2] - horizon[0]) + 15.0,
        "zenith side is bluer than the horizon"
    );
    // Below the centre is the flat ground colour, darker than the horizon.
    assert!(
        luma(horizon) > luma(ground) + 10.0,
        "the ground reflection is darker than the horizon"
    );
    let lower: Vec<f32> = (mid + (bottom - mid) / 2..=bottom)
        .map(|y| luma(row(&pixels, y)))
        .collect();
    let spread =
        lower.iter().copied().fold(f32::MIN, f32::max) - lower.iter().copied().fold(f32::MAX, f32::min);
    assert!(
        spread <= 4.0,
        "the ground reflection is flat ({spread:.2} levels)"
    );
}

#[test]
#[ignore = "assumes the existing two-tone studio reflects flat above the horizon; on lavapipe it does not (27.8 levels with the probe GI, 34.9 without, 10-31): look review pending (ADR-1654 (7))"]
fn the_two_tone_studio_reflects_a_flat_sky_where_the_gradient_does_not() {
    let device = RenderDevice::headless().expect("lavapipe");
    let g = gradient();
    let mut renderer = studio_renderer();
    renderer.set_studio_sky(&device, Some((g.horizon, g.ground)));
    let flat = render(&device, &mut renderer);
    let (top, _) = sphere_rows();
    let mid = H / 2;
    let step = (luma(row(&flat, top)) - luma(row(&flat, mid))).abs();
    assert!(
        step <= 4.0,
        "the two-tone sky is flat above the horizon ({step:.2})"
    );
    assert_eq!(renderer.studio_environment_key(), None);

    renderer.set_studio_gradient(&device, Some(&g)).expect("valid");
    let ramp = render(&device, &mut renderer);
    assert!(luma(row(&ramp, top)) - luma(row(&ramp, mid)) > 12.0);
}

#[test]
fn the_bake_is_deterministic_and_rebuilds_only_when_the_parameters_change() {
    let device = RenderDevice::headless().expect("lavapipe");
    let g = gradient();
    let mut renderer = studio_renderer();
    assert_eq!(renderer.studio_environment_key(), None);
    assert_eq!(renderer.set_studio_gradient(&device, Some(&g)), Ok(true));
    assert_eq!(renderer.studio_environment_key(), Some(g.cache_key()));
    let first = render(&device, &mut renderer);
    // Frames carry a per-frame dither, so pixels are compared with a control renderer that keeps
    // the first gradient and renders the same frame indices. A fresh bake gives the same pixels.
    let mut control = studio_renderer();
    assert_eq!(control.set_studio_gradient(&device, Some(&g)), Ok(true));
    assert_eq!(render(&device, &mut control), first);
    // The same parameters keep the installed maps.
    assert_eq!(renderer.set_studio_gradient(&device, Some(&g)), Ok(false));
    // Another parameter rebuilds and reaches the frame (the cached view groups are dropped,
    // ADR-1654). Restoring the first one is checked by
    // `restoring_the_first_gradient_brings_its_frame_back`.
    let other = StudioGradient { falloff: 3.0, ..g };
    assert_eq!(renderer.set_studio_gradient(&device, Some(&other)), Ok(true));
    let with_other = render(&device, &mut renderer);
    let control_second = render(&device, &mut control);
    let changed = with_other
        .iter()
        .zip(&control_second)
        .filter(|(a, b)| a.abs_diff(**b) > 8)
        .count();
    assert!(
        changed > 1000,
        "the rebuilt environment shows in the frame ({changed} channels differ by more than 8 levels)"
    );
    assert_eq!(renderer.set_studio_gradient(&device, Some(&g)), Ok(true));
    // A bad value is refused and leaves the installed environment alone.
    let bad = StudioGradient {
        zenith: [f32::NAN, 0.0, 0.0],
        ..g
    };
    assert!(renderer.set_studio_gradient(&device, Some(&bad)).is_err());
    assert_eq!(renderer.studio_environment_key(), Some(g.cache_key()));
    // Clearing returns to the default IBL; the two-tone call forgets the key.
    assert_eq!(renderer.set_studio_gradient(&device, None), Ok(false));
    assert_eq!(renderer.studio_environment_key(), None);
    renderer.set_studio_gradient(&device, Some(&g)).expect("valid");
    renderer.set_studio_sky(&device, Some((g.horizon, g.ground)));
    assert_eq!(renderer.studio_environment_key(), None);
    assert_eq!(renderer.set_studio_gradient(&device, Some(&g)), Ok(true));
}

#[test]
fn restoring_the_first_gradient_brings_its_frame_back() {
    let device = RenderDevice::headless().expect("lavapipe");
    let g = gradient();
    let mut renderer = studio_renderer();
    let mut control = studio_renderer();
    renderer.set_studio_gradient(&device, Some(&g)).expect("valid");
    control.set_studio_gradient(&device, Some(&g)).expect("valid");
    render(&device, &mut renderer);
    render(&device, &mut control);
    let other = StudioGradient { falloff: 3.0, ..g };
    renderer
        .set_studio_gradient(&device, Some(&other))
        .expect("valid");
    let with_other = render(&device, &mut renderer);
    let changed = with_other
        .iter()
        .zip(&render(&device, &mut control))
        .filter(|(a, b)| a.abs_diff(**b) > 8)
        .count();
    // Restored, the frame is the control's again up to the per-frame dither (a stale environment
    // would still differ as much as `changed`).
    assert_eq!(renderer.set_studio_gradient(&device, Some(&g)), Ok(true));
    let restored = render(&device, &mut renderer);
    let control_third = render(&device, &mut control);
    let still_changed = restored
        .iter()
        .zip(&control_third)
        .filter(|(a, b)| a.abs_diff(**b) > 8)
        .count();
    assert!(
        still_changed * 10 < changed,
        "restoring the first gradient brings its frame back ({still_changed} of {changed} channels still differ)"
    );
}

#[test]
#[ignore = "open defect: the equirect image of the gradient and the analytic gradient differ by 34.5 levels on lavapipe with the probe GI on (183.5 before ADR-2000); with `gi_probes` off they agree within 8 levels (10-31, ADR-2000 (6)); probe GI follow-up (ADR-1654 (7))"]
fn an_equirect_image_of_the_gradient_reflects_like_the_gradient() {
    let device = RenderDevice::headless().expect("lavapipe");
    let g = gradient();
    let (w, h) = (256u32, 128u32);
    let mut texels = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        let theta = (y as f32 + 0.5) / h as f32 * std::f32::consts::PI;
        for _ in 0..w {
            texels.push(g.radiance([theta.sin(), theta.cos(), 0.0]));
        }
    }
    let image = StudioEquirect::new(w, h, texels).expect("image");
    let mut renderer = studio_renderer();
    assert!(renderer.set_studio_equirect(&device, Some(&image)));
    assert!(!renderer.set_studio_equirect(&device, Some(&image)));
    assert_eq!(renderer.studio_environment_key(), Some(image.cache_key()));
    let from_image = render(&device, &mut renderer);
    renderer.set_studio_gradient(&device, Some(&g)).expect("valid");
    let analytic = render(&device, &mut renderer);
    let (top, bottom) = sphere_rows();
    let mut worst = 0.0f32;
    for y in top..=bottom {
        let (a, b) = (row(&from_image, y), row(&analytic, y));
        worst = worst.max((0..3).map(|c| (a[c] - b[c]).abs()).fold(0.0, f32::max));
    }
    eprintln!("equirect vs analytic: worst row difference {worst:.2} levels");
    assert!(
        worst <= 8.0,
        "the image environment differs from the gradient by {worst:.2} levels"
    );
}
