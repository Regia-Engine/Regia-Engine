// SPDX-License-Identifier: MPL-2.0
//! Authored area lights and sized spots in the path tracer (ADR-2592): a `RectLight` and a `TubeLight` added with
//! `extract::push_area_lights` light a Lambert plane as the forward pass's LTC integration says
//! (`emissive_lights::irradiance_at`, the CPU mirror of `forward_ltc.wgsl`'s polygon integral), and a sized spot
//! (`PtLight::SphereSpot`) keeps its cone. Runs on whatever adapter `RenderDevice::headless` picks (lavapipe in CI).

use std::sync::OnceLock;

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_pt::extract::{push_area_lights, push_local_lights};
use regia_pt::scenes;
use regia_pt::{PathTracer, PtCamera, PtImage, PtMaterial, PtScene, PtSettings};
use regia_render::light_profiles::{AreaLightInput, rect_emitter, tube_emitter};
use regia_render::{LabLocalLight, LabSpotLight, RectLight, RenderDevice, TubeLight};

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

fn render(scene: &PtScene, spp: u32) -> PtImage {
    let settings = PtSettings {
        spp,
        max_bounces: 1,
        seed: 7,
        rr_start: 3,
        jitter: false,
        samples_per_submit: 64,
    };
    tracer().render(scene, &settings).expect("render")
}

/// Camera ray through the centre of pixel (x, y) (the `generate` kernel's pinhole).
fn pixel_ray(cam: &PtCamera, x: u32, y: u32) -> (Vec3, Vec3) {
    let (fwd, right, up) = cam.basis();
    let th = (0.5 * cam.fov_y).tan();
    let aspect = cam.width as f32 / cam.height as f32;
    let sx = ((x as f32 + 0.5) / cam.width as f32) * 2.0 - 1.0;
    let sy = 1.0 - ((y as f32 + 0.5) / cam.height as f32) * 2.0;
    let d = (fwd + right * (sx * th * aspect) + up * (sy * th)).normalize();
    (Vec3::from_array(cam.eye), d)
}

const RHO: f32 = 0.6;

fn plane_scene(cam: PtCamera) -> PtScene {
    let mut scene = PtScene::new(cam);
    let plane = scene.add_mesh(scenes::quad([
        [-20.0, 0.0, -20.0],
        [20.0, 0.0, -20.0],
        [20.0, 0.0, 20.0],
        [-20.0, 0.0, 20.0],
    ]));
    let mat = scene.add_material(PtMaterial::lambert([RHO; 3]));
    scene.add_instance(plane, mat, Mat4::IDENTITY);
    scene
}

/// Mean relative error and bias of the traced direct term against `expect(p)` (irradiance, lux) over the plane
/// pixels the light lights well.
fn compare(cam: &PtCamera, img: &PtImage, expect: impl Fn(Vec3) -> f32) -> (f64, f64, usize) {
    let (fwd, _, _) = cam.basis();
    let (mut err, mut bias, mut n) = (0.0f64, 0.0f64, 0usize);
    for y in 0..cam.height {
        for x in 0..cam.width {
            let (o, d) = pixel_ray(cam, x, y);
            if d.y >= -1e-3 {
                continue;
            }
            let p = o + d * (-o.y / d.y);
            let i = (y * cam.width + x) as usize;
            let plane_depth = (p - o).dot(fwd);
            if (img.depth[i] - plane_depth).abs() > 1e-3 * plane_depth {
                continue;
            }
            let want = f64::from(RHO) / std::f64::consts::PI * f64::from(expect(p));
            if want < 0.05 {
                continue;
            }
            let rel = f64::from(img.direct[i][1]) / want - 1.0;
            err += rel.abs();
            bias += rel;
            n += 1;
        }
    }
    (err / n.max(1) as f64, bias / n.max(1) as f64, n)
}

/// PT against raster for a rect light: the traced direct light on a Lambert plane under an authored
/// `RectLight` matches the LTC integral the forward pass shades it with.
#[test]
fn a_rect_light_traces_as_the_forward_ltc_integral_says() {
    let cam = PtCamera::pinhole([0.0, 2.0, 3.5], [0.0, 0.0, 0.0], 0.9, 32, 20);
    let rect = RectLight {
        width: 1.2,
        height: 0.6,
        intensity: 900.0,
        ..RectLight::default()
    };
    // Emitting toward its −Z: turned to face the floor, 1.4 m up.
    let model = Mat4::from_rotation_translation(
        Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        Vec3::new(0.2, 1.4, -0.3),
    );
    let light = rect_emitter(&rect, &model);
    let mut scene = plane_scene(cam);
    push_area_lights(
        &mut scene,
        &[AreaLightInput {
            light,
            range: 0.0,
            cast_shadows: true,
        }],
    );
    let img = render(&scene, 1024);
    let (mre, bias, n) = compare(&cam, &img, |p| {
        regia_render::emissive_lights::irradiance_at(&light, p, Vec3::Y)
    });
    eprintln!("rect light: {n} px, mean relative error {mre:.5}, bias {bias:.5}");
    assert!(n > 200, "{n} pixels");
    assert!(bias.abs() < 0.005, "bias {bias}");
    assert!(mre < 0.02, "mean relative error {mre}");
}

/// A tube light traces within the prism's tessellation of the forward's facing-quad LTC tube.
#[test]
fn a_tube_light_traces_close_to_the_forward_tube() {
    let cam = PtCamera::pinhole([0.0, 2.0, 3.5], [0.0, 0.0, 0.0], 0.9, 32, 20);
    let tube = TubeLight {
        length: 1.5,
        radius: 0.03,
        intensity: 1200.0,
        ..TubeLight::default()
    };
    let light = tube_emitter(&tube, &Mat4::from_translation(Vec3::new(0.0, 1.2, 0.0)));
    let mut scene = plane_scene(cam);
    push_area_lights(
        &mut scene,
        &[AreaLightInput {
            light,
            range: 0.0,
            cast_shadows: false,
        }],
    );
    let img = render(&scene, 1024);
    let (mre, bias, n) = compare(&cam, &img, |p| {
        regia_render::emissive_lights::irradiance_at(&light, p, Vec3::Y)
    });
    eprintln!("tube light: {n} px, mean relative error {mre:.5}, bias {bias:.5}");
    assert!(n > 200, "{n} pixels");
    assert!(bias.abs() < 0.02, "bias {bias}");
    assert!(mre < 0.04, "mean relative error {mre}");
}

/// A sized spot is a cone-limited sphere: inside the cone it lights the plane as the delta spot of the same
/// intensity does (to the sphere's small-source error), outside it lights nothing.
#[test]
fn a_sized_spot_keeps_its_cone_and_its_intensity() {
    let cam = PtCamera::pinhole([0.0, 3.0, 4.0], [0.0, 0.0, 0.0], 0.9, 48, 32);
    let spot = |source_radius: Option<f32>| {
        LabLocalLight::Spot(LabSpotLight {
            position: [0.0, 2.0, 0.0],
            direction: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            intensity: 200.0,
            range: 50.0,
            inner_angle: 0.6,
            outer_angle: 0.8,
            cast_shadows: false,
            source_radius,
        })
    };
    let mut sized = plane_scene(cam);
    push_local_lights(&mut sized, &[spot(Some(0.1))]);
    let mut delta = plane_scene(cam);
    push_local_lights(&mut delta, &[spot(None)]);
    let a = render(&sized, 256);
    let b = render(&delta, 16);
    let (mut inside, mut outside, mut sum_a, mut sum_b) = (0usize, 0usize, 0.0f64, 0.0f64);
    for y in 0..cam.height {
        for x in 0..cam.width {
            let (o, d) = pixel_ray(&cam, x, y);
            if d.y >= -1e-3 {
                continue;
            }
            let p = o + d * (-o.y / d.y);
            let i = (y * cam.width + x) as usize;
            let cos = (p - Vec3::new(0.0, 2.0, 0.0)).normalize().dot(Vec3::NEG_Y);
            if cos > 0.6f32.cos() + 0.01 {
                inside += 1;
                sum_a += f64::from(a.direct[i][1]);
                sum_b += f64::from(b.direct[i][1]);
            } else if cos < 0.8f32.cos() - 0.05 {
                outside += 1;
                assert!(a.direct[i][1] < 1e-4, "lit outside the cone: {}", a.direct[i][1]);
            }
        }
    }
    assert!(inside > 20 && outside > 20, "{inside} / {outside}");
    let ratio = sum_a / sum_b;
    eprintln!("sized spot: {inside} px inside, sized / delta {ratio:.4}");
    assert!((ratio - 1.0).abs() < 0.03, "sized / delta {ratio}");
}

/// An IES profile on a traced spot (`PtLightProfile`, `extract::push_light_profiles`): the profiled image over the
/// plain one is, at every lit plane pixel, the CPU profile lookup toward that pixel.
#[test]
fn an_ies_spot_traces_as_the_cpu_profile_says() {
    use regia_render::light_profiles::{LightExtras, profile_use, relative_gpu};
    let cam = PtCamera::pinhole([0.0, 5.0, 5.0], [0.0, 0.0, 0.0], 1.0, 32, 20);
    let file = regia_asset::ies::IesFile::parse(&regia_asset::ies::example_text()).expect("parse");
    let profile = std::sync::Arc::new(regia_asset::ies::CookedIesProfile::from_file(&file).expect("cook"));
    let light_pos = Vec3::new(0.0, 2.5, 0.0);
    let lights = [LabLocalLight::Spot(LabSpotLight {
        position: light_pos.to_array(),
        direction: [0.0, -1.0, 0.0],
        color: [1.0; 3],
        intensity: 300.0,
        range: 60.0,
        inner_angle: 1.3,
        outer_angle: 1.4,
        cast_shadows: false,
        source_radius: None,
    })];
    let mut plain = plane_scene(cam);
    push_local_lights(&mut plain, &lights);
    let mut shaped = plane_scene(cam);
    push_local_lights(&mut shaped, &lights);
    let mut extras = LightExtras::default();
    let uses = profile_use(Vec3::NEG_Y, Vec3::X, 0.6, None);
    assert!(extras.assign(0, &profile, uses));
    regia_pt::extract::push_light_profiles(&mut shaped, 0, &lights, &extras);
    assert_eq!(shaped.light_profiles.len(), 1);
    let a = render(&plain, 4);
    let b = render(&shaped, 4);
    let code = 1u32 | (u32::from(uses.turn) << 3);
    let (mut n, mut worst, mut dark) = (0usize, 0.0f64, 0usize);
    for y in 0..cam.height {
        for x in 0..cam.width {
            let (o, d) = pixel_ray(&cam, x, y);
            if d.y >= -1e-3 {
                continue;
            }
            let p = o + d * (-o.y / d.y);
            let i = (y * cam.width + x) as usize;
            let base = f64::from(a.direct[i][1]);
            if base < 1e-3 {
                continue;
            }
            let want = f64::from(relative_gpu(
                &profile,
                code,
                Vec3::NEG_Y,
                (p - light_pos).normalize(),
            ));
            if want < 1e-3 {
                dark += 1;
            }
            worst = worst.max((f64::from(b.direct[i][1]) / base - want).abs());
            n += 1;
        }
    }
    eprintln!("ies spot (pt): {n} px, {dark} outside the profile, worst |ratio − profile| {worst:.5}");
    assert!(n > 200 && dark > 20, "{n} / {dark}");
    assert!(worst < 0.01, "worst {worst}");
}
