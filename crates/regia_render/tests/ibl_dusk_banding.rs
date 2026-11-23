// SPDX-License-Identifier: MPL-2.0
//! The sky IBL holds a dusk sky without banding (look-r2 round 3).
//!
//! At Mars dusk the sky radiance is a few hundred cd/m²; the IBL stored it as RGBA8 over the
//! 9000 cd/m² scale, i.e. ~8 levels for the whole sky, so a smooth diffuse sphere lit by it
//! showed stair steps in its ambient gradient. The profile down the sphere's middle (mean of
//! a few columns per row) must be smooth: no row-to-row second difference larger than a
//! dithered 8-bit gradient makes.

use regia_render::{
    AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode, TimeOfDay,
    horizontal_plus_fov_y, sun_position, translate_mesh, uv_sphere,
};

const W: u32 = 256;
const H: u32 = 256;

fn render(device: &RenderDevice) -> Vec<u8> {
    let (v, i) = uv_sphere(3.0, 128, 64);
    let time = TimeOfDay {
        hour: 17.75,
        day_of_year: 100,
        latitude_deg: 20.0,
    };
    let toward = sun_position(&time).direction;
    // Look at the sphere from the side away from the sun: the visible half is lit by the
    // sky alone (the sun term ends at the limb, outside the measured rows).
    let flat = {
        let l = (toward[0] * toward[0] + toward[2] * toward[2]).sqrt().max(1e-4);
        [toward[0] / l, toward[2] / l]
    };
    let eye = [-flat[0] * 9.0, 2.0, -flat[1] * 9.0];
    let post = PostSettings {
        auto_exposure: false,
        // Dusk sky light: the sphere sits in the mid tones.
        exposure_ev: 9.5,
        bloom: false,
        ..PostSettings::default()
    };
    let lab = LabScene {
        width: W,
        height: H,
        eye,
        target: [0.0, 2.0, 0.0],
        fov_y: 0.75,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 9.5,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new([-toward[0], -toward[1], -toward[2]], [1.0; 3], 0.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 1.0,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: translate_mesh(&v, [0.0, 2.0, 0.0]),
            indices: i,
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
    let mut outdoor = OutdoorParams {
        time: Some(time),
        ..OutdoorParams::default()
    };
    outdoor.planet = regia_render::AtmosphereParameters::from_preset(AtmospherePreset::Mars);
    renderer.set_outdoor(outdoor);
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut pixels = Vec::new();
    for _ in 0..2 {
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    pixels
}

#[test]
fn a_dusk_sky_lights_a_sphere_without_banding() {
    let device = RenderDevice::headless().expect("lavapipe");
    let pixels = render(&device);
    if regia_core::brand::env_var_os("REGIA_WRITE_EVIDENCE").is_some() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/look_r2_tests");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("ibl_dusk_sphere.rgba"), &pixels);
    }
    let luma = |x: u32, y: u32| {
        let i = ((y * W + x) * 4) as usize;
        0.2126 * f32::from(pixels[i]) + 0.7152 * f32::from(pixels[i + 1]) + 0.0722 * f32::from(pixels[i + 2])
    };
    // The sphere is centred on the look axis, radius 3 at 9 m. Its silhouette is
    // asin(3/9) off axis. Desktop High widens the vertical field on this square
    // view. The background below the horizon is the planet, so a luma cut against
    // the left edge no longer finds that silhouette.
    let cx = W / 2;
    let fov_y = horizontal_plus_fov_y(0.75, W as f32 / H as f32);
    let half_fov = fov_y * 0.5;
    let angular = (3.0_f32 / 9.0).asin();
    let ndc = angular.tan() / half_fov.tan();
    let half_rows = (ndc * (H as f32) * 0.5).floor() as u32;
    let mid = H / 2;
    let top = mid.saturating_sub(half_rows);
    let bottom = (mid + half_rows).min(H - 1);
    assert!(
        bottom - top > 80,
        "the sphere fills the frame ({} rows)",
        bottom - top
    );
    assert!(
        (luma(cx, mid) - luma(4, 4)).abs() > 12.0,
        "the sphere should not match the sky"
    );
    let span = bottom - top;
    let (y0, y1) = (top + span / 10, bottom - span / 10);
    let profile: Vec<f32> = (y0..y1)
        .map(|y| (cx - 4..cx + 4).map(|x| luma(x, y)).sum::<f32>() / 8.0)
        .collect();
    let mean = profile.iter().sum::<f32>() / profile.len() as f32;
    let worst = profile
        .windows(3)
        .map(|w| (w[2] - 2.0 * w[1] + w[0]).abs())
        .fold(0.0f32, f32::max);
    eprintln!(
        "dusk sphere: mean luma {mean:.1}, rows {}, worst second difference {worst:.2}",
        profile.len()
    );
    assert!(
        mean > 25.0 && mean < 230.0,
        "sphere is exposed in the mid tones ({mean:.1})"
    );
    // A quantization step is a jump: |second difference| about twice the step (RGBA8 gave
    // 6.6 here). The bilinear 64×32 irradiance map leaves slope kinks of ~2 levels at its
    // texel boundaries, which read as a smooth gradient.
    assert!(
        worst <= 3.0,
        "the sky-lit gradient steps by {worst:.2} levels between rows: the IBL is banding"
    );
}
