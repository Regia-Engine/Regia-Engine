// SPDX-License-Identifier: MPL-2.0
//! Bake light inputs (ADR-1095) against closed-form answers: equirect HDRI skies (constant,
//! hemispherical, smooth and rotated, with an extracted sun), area emitters read from a
//! `.bake.ron` (disc, quad, sphere; one- and two-sided), and HDRI sky and sun light bounced off
//! a Lambertian ground into the right components. Runs the real kernels on whatever adapter
//! `RenderDevice::headless` picks (lavapipe in CI).

use std::f32::consts::PI;
use std::path::Path;
use std::sync::OnceLock;

use regia_asset::baked_gi::{GiComponent, SH_Y1, ShRgb};
use regia_core::glam::{Mat4, Vec3};
use regia_pt::analytic::{
    add_sh, disc_projected_solid_angle, disc_solid_angle, hemisphere_sh, lambert_emitter_luminance,
    rect_projected_solid_angle, rect_solid_angle, sphere_projected_solid_angle, sphere_solid_angle,
    symmetric_emitter_sh, uniform_sh,
};
use regia_pt::bake_desc::GiBakeDescription;
use regia_pt::env_map::{EnvMap, rotate_y, sky_irradiance, sky_sh};
use regia_pt::probe::{ProbeTrace, ProbeTraceSettings};
use regia_pt::scenes::quad;
use regia_pt::{PathTracer, PtCamera, PtMaterial, PtScene, PtSky};
use regia_render::RenderDevice;

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

fn settings(rays: u32, samples: u32, bounces: u32) -> ProbeTraceSettings {
    ProbeTraceSettings {
        rays_per_probe: rays,
        samples,
        max_bounces: bounces,
        seed: 7,
        rr_start: 64,
        samples_per_submit: 16,
        ..ProbeTraceSettings::default()
    }
}

fn trace(scene: &PtScene, positions: &[[f32; 3]], s: &ProbeTraceSettings) -> Vec<ProbeTrace> {
    tracer()
        .trace_probes(scene, positions, s, |_, _| true)
        .expect("trace probes")
}

fn camera() -> PtCamera {
    PtCamera::pinhole([0.0, 0.0, 5.0], [0.0; 3], 0.5, 4, 4)
}

fn comp(t: &ProbeTrace, c: GiComponent) -> &ShRgb {
    &t.sh[c as usize]
}

/// Every coefficient of `got` within `rel` × the reference's L00 of `want` (per channel).
fn assert_sh(what: &str, got: &ShRgb, want: &ShRgb, rel: f32) {
    for (ch, (g, w)) in got.iter().zip(want).enumerate() {
        let scale = w[0].abs().max(1e-6);
        for (k, (a, b)) in g.iter().zip(w).enumerate() {
            assert!(
                (a - b).abs() <= rel * scale,
                "{what}: channel {ch} coefficient {k}: got {g:?}, want {w:?} (± {rel} × L00)"
            );
        }
    }
}

fn zero(t: &ProbeTrace, c: GiComponent) -> bool {
    comp(t, c).iter().flatten().all(|v| *v == 0.0)
}

/// A 2 km Lambertian ground at y = 0 facing up.
fn add_ground(scene: &mut PtScene, albedo: f32) {
    let mesh = scene.add_mesh(quad([
        [-1000.0, 0.0, -1000.0],
        [-1000.0, 0.0, 1000.0],
        [1000.0, 0.0, 1000.0],
        [1000.0, 0.0, -1000.0],
    ]));
    let m = scene.add_material(PtMaterial::lambert([albedo; 3]));
    scene.add_instance(mesh, m, Mat4::IDENTITY);
}

#[test]
fn a_constant_hdri_bakes_like_the_uniform_sky() {
    let c = [1.0f32, 0.5, 0.25];
    let mut scene = PtScene::new(camera());
    scene.sky = EnvMap::constant(64, 32, c).into_sky(0.3, 2.0).expect("sky");
    let s = settings(128, 8, 2);
    let t = &trace(&scene, &[[0.0; 3], [5.0, 1.0, -3.0]], &s)[1];
    assert_eq!((t.rejected, t.hits), (0, 0));
    let want = uniform_sh([2.0 * c[0], 2.0 * c[1], 2.0 * c[2]]);
    assert_sh("constant hdri", comp(t, GiComponent::Sky), &want, 2e-3);
    let mut uniform = scene.clone();
    uniform.sky = PtSky::Uniform([2.0 * c[0], 2.0 * c[1], 2.0 * c[2]]);
    let u = &trace(&uniform, &[[5.0, 1.0, -3.0]], &s)[0];
    assert_sh(
        "hdri vs uniform",
        comp(t, GiComponent::Sky),
        comp(u, GiComponent::Sky),
        2e-3,
    );
    for k in [GiComponent::SunBounce, GiComponent::Fill, GiComponent::Emissive] {
        assert!(zero(t, k), "{k:?}");
    }
}

#[test]
fn an_upper_hemisphere_hdri_matches_the_analytic_moments() {
    let l = 3.0f32;
    let mut scene = PtScene::new(camera());
    scene.sky = EnvMap::from_fn(256, 128, |d| if d[1] > 0.0 { [l; 3] } else { [0.0; 3] })
        .into_sky(0.0, 1.0)
        .expect("sky");
    let t = &trace(&scene, &[[0.0; 3]], &settings(512, 16, 1))[0];
    let sky = comp(t, GiComponent::Sky);
    eprintln!(
        "hemisphere hdri: {:?} want {:?}",
        sky[0],
        hemisphere_sh([l; 3], [0.0, 1.0, 0.0])[0]
    );
    assert_sh("hemisphere", sky, &hemisphere_sh([l; 3], [0.0, 1.0, 0.0]), 0.015);
}

/// A smooth map with a warm lobe, turned and scaled: the probe's projection is the CPU
/// quadrature of the same lookup, and its first moment points where the turned lobe is.
#[test]
fn a_rotated_hdri_projects_like_its_cpu_quadrature() {
    let lobe = Vec3::new(0.0, 0.3, -1.0).normalize();
    let map = EnvMap::from_fn(256, 128, |d| {
        let k = Vec3::from_array(d).dot(lobe).max(0.0).powi(4);
        [0.2 + 3.0 * k, 0.25 + 2.0 * k, 0.3 + 0.5 * k]
    });
    let rotation = 1.1f32;
    let sky = map.into_sky(rotation, 4.0).expect("sky");
    let want = sky_sh(&sky, 512);
    let mut scene = PtScene::new(camera());
    scene.sky = sky;
    let t = &trace(&scene, &[[0.0; 3]], &settings(512, 32, 1))[0];
    let got = comp(t, GiComponent::Sky);
    eprintln!("rotated hdri: {:?} want {:?}", got[0], want[0]);
    assert_sh("rotated hdri", got, &want, 0.01);
    let l1 = Vec3::new(got[0][1], got[0][2], got[0][3]).normalize();
    let turned = Vec3::from_array(rotate_y(lobe.to_array(), rotation));
    assert!(l1.dot(turned) > 0.98, "L1 {l1} vs the turned lobe {turned}");
}

/// The HDRI's light reaches the ground by next-event estimation over the map's distribution
/// and by BSDF rays (MIS); the probe sees the sky above and the ground's ρ·E/π below, all in
/// the sky component.
#[test]
fn hdri_sky_light_bounced_off_a_lambert_ground_is_rho_e_over_pi() {
    let rho = 0.5f32;
    let map = EnvMap::from_fn(256, 128, |d| {
        if d[1] > 0.0 {
            [0.5 + 1.5 * d[1], 0.7 + d[1], 1.0 + 0.5 * d[0].abs()]
        } else {
            [0.0; 3]
        }
    });
    let sky = map.into_sky(0.4, 100.0).expect("sky");
    let e_up = sky_irradiance(&sky, [0.0, 1.0, 0.0], 512);
    let ground = [rho * e_up[0] / PI, rho * e_up[1] / PI, rho * e_up[2] / PI];
    let want = add_sh(&sky_sh(&sky, 512), &hemisphere_sh(ground, [0.0, -1.0, 0.0]));
    let mut scene = PtScene::new(camera());
    scene.sky = sky;
    add_ground(&mut scene, rho);
    let t = &trace(&scene, &[[0.0, 1.0, 0.0]], &settings(512, 32, 2))[0];
    assert_eq!(t.rejected, 0);
    let got = comp(t, GiComponent::Sky);
    eprintln!("hdri + ground: {:?} want {:?}", got[0], want[0]);
    assert_sh("hdri + ground", got, &want, 0.03);
    for k in [GiComponent::SunBounce, GiComponent::Fill, GiComponent::Emissive] {
        assert!(zero(t, k), "{k:?}");
    }
}

/// An HDRI holding a hot sun disc: extraction lifts it into the sun, whose light reaches the
/// probe only after the ground bounce (sun component, ρ·E·sin(elevation)/π below), while the
/// rest of the map stays in the sky component.
#[test]
fn an_extracted_hdri_sun_bakes_its_bounce_and_leaves_the_sky() {
    let rho = 0.4f32;
    let sun_dir = Vec3::new(0.5, 0.7, -0.4).normalize();
    let mut map = EnvMap::from_fn(512, 256, |d| {
        let v = Vec3::from_array(d);
        if v.dot(sun_dir) > 1.5f32.to_radians().cos() {
            [400_000.0, 380_000.0, 350_000.0]
        } else if d[1] > 0.0 {
            [0.6, 0.8, 1.0]
        } else {
            [0.0; 3]
        }
    });
    let sun = map.extract_sun(100.0, 0.03).expect("extract").expect("a sun");
    let rotation = -0.7f32;
    let scale = 10.0f32;
    let pt_sun = sun.to_sun(rotation, scale);
    let sky = map.into_sky(rotation, scale).expect("sky");
    let toward = Vec3::from_array(pt_sun.toward);
    assert!(toward.dot(Vec3::from_array(rotate_y(sun_dir.to_array(), rotation))) > 0.999);
    let mut scene = PtScene::new(camera());
    let e_up = sky_irradiance(&sky, [0.0, 1.0, 0.0], 512);
    let want_sky = add_sh(
        &sky_sh(&sky, 512),
        &hemisphere_sh(e_up.map(|e| rho * e / PI), [0.0, -1.0, 0.0]),
    );
    let e_sun = pt_sun.illuminance * toward.y;
    let want_sun = hemisphere_sh(pt_sun.color.map(|c| rho * c * e_sun / PI), [0.0, -1.0, 0.0]);
    scene.sky = sky;
    scene.sun = Some(pt_sun);
    add_ground(&mut scene, rho);
    let t = &trace(&scene, &[[0.0, 1.0, 0.0]], &settings(512, 32, 2))[0];
    assert_eq!(t.rejected, 0);
    let got_sun = comp(t, GiComponent::SunBounce);
    eprintln!("extracted sun bounce: {:?} want {:?}", got_sun[0], want_sun[0]);
    assert_sh("sun bounce", got_sun, &want_sun, 0.03);
    // No direct sun in the bake: the upward first moment is the ground's, pointing down.
    assert!(got_sun[1][2] < 0.0);
    let got_sky = comp(t, GiComponent::Sky);
    eprintln!("extracted sky: {:?} want {:?}", got_sky[0], want_sky[0]);
    assert_sh("sky after extraction", got_sky, &want_sky, 0.03);
}

fn emitter_scene(emitters: &str) -> PtScene {
    let text = format!(
        "(format_version: 2, emitters: [{emitters}], layout: (bricks: [(min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0), spacing: 1.0)]))"
    );
    GiBakeDescription::from_ron(&text)
        .expect("parse")
        .build(Path::new("."))
        .expect("build")
        .scene
}

#[test]
fn a_disc_emitter_matches_the_disc_solid_angle_and_first_moment() {
    let (r, h, l) = (0.5f32, 2.0f32, 10.0f32);
    let scene = emitter_scene(
        "(shape: Disc(center: (0.0, 2.0, 0.0), normal: (0.0, -1.0, 0.0), radius: 0.5, segments: 512), radiance: Some((10.0, 10.0, 10.0)))",
    );
    let s = settings(1024, 32, 1);
    let t = trace(&scene, &[[0.0; 3], [0.0, 4.0, 0.0]], &s);
    let want = symmetric_emitter_sh(
        [l; 3],
        disc_solid_angle(r, h),
        disc_projected_solid_angle(r, h),
        [0.0, 1.0, 0.0],
    );
    let got = comp(&t[0], GiComponent::Emissive);
    eprintln!("disc: {:?} want {:?}", got[0], want[0]);
    assert_sh("disc below", got, &want, 0.02);
    // The disc faces down: from above it emits nothing (it still occludes).
    assert!(
        zero(&t[1], GiComponent::Emissive),
        "{:?}",
        comp(&t[1], GiComponent::Emissive)
    );
    assert!(t[1].hits > 0);

    let two = emitter_scene(
        "(shape: Disc(center: (0.0, 2.0, 0.0), normal: (0.0, -1.0, 0.0), radius: 0.5, segments: 512), radiance: Some((10.0, 10.0, 10.0)), two_sided: true)",
    );
    let t = trace(&two, &[[0.0, 4.0, 0.0]], &s);
    let above = symmetric_emitter_sh(
        [l; 3],
        disc_solid_angle(r, h),
        disc_projected_solid_angle(r, h),
        [0.0, -1.0, 0.0],
    );
    assert_sh(
        "two-sided disc above",
        comp(&t[0], GiComponent::Emissive),
        &above,
        0.02,
    );
}

#[test]
fn a_quad_emitter_given_in_lumens_matches_the_rectangle_formulas() {
    // A 1 m × 0.5 m panel 1.5 m in front of the probe along −Z, facing +Z, 300 lm.
    let scene = emitter_scene(
        "(shape: Quad(corner: (-0.5, -0.25, -1.5), edge_u: (1.0, 0.0, 0.0), edge_v: (0.0, 0.5, 0.0)), power: Some(300.0))",
    );
    let l = lambert_emitter_luminance(300.0, 0.5, false);
    let want = symmetric_emitter_sh(
        [l; 3],
        rect_solid_angle(0.5, 0.25, 1.5),
        rect_projected_solid_angle(0.5, 0.25, 1.5),
        [0.0, 0.0, -1.0],
    );
    let t = &trace(&scene, &[[0.0; 3]], &settings(1024, 32, 1))[0];
    let got = comp(t, GiComponent::Emissive);
    eprintln!("quad: {:?} want {:?}", got[0], want[0]);
    assert_sh("quad", got, &want, 0.02);
}

#[test]
fn a_sphere_emitter_matches_its_cone() {
    let (r, d, l) = (0.3f32, 2.0f32, 40.0f32);
    let scene = emitter_scene(
        "(shape: Sphere(center: (2.0, 0.0, 0.0), radius: 0.3, segments: 128), radiance: Some((40.0, 30.0, 20.0)))",
    );
    // The sphere covers 0.6 % of the directions: 32 k rays put about 190 on it and scatter L00
    // by ±2 % from seed to seed; 2 M rays bring that to a few tenths of a percent.
    let t = &trace(&scene, &[[0.0; 3]], &settings(4096, 512, 1))[0];
    let want = symmetric_emitter_sh(
        [l, 30.0, 20.0],
        sphere_solid_angle(r, d),
        sphere_projected_solid_angle(r, d),
        [1.0, 0.0, 0.0],
    );
    let got = comp(t, GiComponent::Emissive);
    eprintln!("sphere: {:?} want {:?}", got[0], want[0]);
    assert_sh("sphere", got, &want, 0.02);
    // The first moment of a cone is exact: π·sin²(half angle)·Y1.
    let half = (r / d).asin();
    assert!((got[0][1] - SH_Y1 * l * PI * half.sin().powi(2)).abs() < 0.02 * got[0][0]);
}

/// An emitter's light bounces off the ground into the emissive component: a large two-sided
/// panel high above makes the ground's radiance ρ·E/π with E the panel's irradiance there.
#[test]
fn emitter_light_bounced_off_the_ground_stays_in_the_emissive_component() {
    let rho = 0.5f32;
    let l = 20.0f32;
    // A 400 m × 400 m panel 20 m up facing down: from the ground near the origin it is almost
    // the whole upper hemisphere, E = L·Ω⊥ of the rectangle.
    let mut scene = emitter_scene(
        "(shape: Quad(corner: (-200.0, 20.0, -200.0), edge_u: (400.0, 0.0, 0.0), edge_v: (0.0, 0.0, 400.0)), radiance: Some((20.0, 20.0, 20.0)))",
    );
    add_ground(&mut scene, rho);
    let t = &trace(&scene, &[[0.0, 1.0, 0.0]], &settings(512, 32, 2))[0];
    let got = comp(t, GiComponent::Emissive);
    let e_ground = l * rect_projected_solid_angle(200.0, 200.0, 20.0);
    let below = hemisphere_sh([rho * e_ground / PI; 3], [0.0, -1.0, 0.0]);
    let above = symmetric_emitter_sh(
        [l; 3],
        rect_solid_angle(200.0, 200.0, 19.0),
        rect_projected_solid_angle(200.0, 200.0, 19.0),
        [0.0, 1.0, 0.0],
    );
    let want = add_sh(&above, &below);
    eprintln!("emitter + ground: {:?} want {:?}", got[0], want[0]);
    assert_sh("emitter + ground", got, &want, 0.03);
    assert!(zero(t, GiComponent::Sky) && zero(t, GiComponent::SunBounce));
}

/// `sky_atmosphere` from a description: the probe's projection of the planet's sky-view table
/// equals the CPU quadrature of the same lookup, and the table follows the sun.
#[test]
fn an_atmosphere_sky_from_a_description_projects_like_its_cpu_quadrature() {
    let text = "(format_version: 2, sun: Some((toward: (0.4, 0.35, -0.6), illuminance: 100000.0)), \
                sky_atmosphere: Some((planet: Earth)), \
                layout: (bricks: [(min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0), spacing: 1.0)]))";
    let mut scene = GiBakeDescription::from_ron(text)
        .expect("parse")
        .build(Path::new("."))
        .expect("build")
        .scene;
    // The direct sun is live: no geometry, so nothing bounces it into the bake.
    scene.sun = None;
    // The table's aureole is narrow: 512 rows read L00 0.7 % low, 1024 and 2048 rows agree to
    // 0.07 %.
    let want = sky_sh(&scene.sky, 2048);
    // 16 k rays scatter L00 by about ±1 % from seed to seed; 256 k rays by a few tenths.
    let t = &trace(&scene, &[[0.0, 2.0, 0.0]], &settings(2048, 128, 1))[0];
    let got = comp(t, GiComponent::Sky);
    eprintln!("atmosphere: {:?} want {:?}", got[2], want[2]);
    assert_sh("atmosphere sky", got, &want, 0.015);
}
