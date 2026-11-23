// SPDX-License-Identifier: MPL-2.0
//! The planet's air as the path tracer traces it (`extract::media_from_outdoor`) against the
//! real-time aerial-perspective table (`regia_render::sample_aerial`) on open, flat ground:
//! a camera looking level at a black wall, so every pixel is the air's in-scatter alone. With
//! no terrain to shadow the air or hide the sky, the two models describe the same light (sun
//! single scattering plus the sky's and the ground's multiple scattering), so their in-scatter
//! must agree; a gap here is a model error, not a scene effect (lane mars-haze, 2026-10-09).

use std::sync::OnceLock;

use regia_core::glam::Mat4;
use regia_pt::extract::media_from_outdoor;
use regia_pt::scenes;
use regia_pt::{PathTracer, PtCamera, PtMaterial, PtScene, PtSettings, PtSky, PtSun};
use regia_render::{
    AtmosphereParameters, OutdoorParams, RenderDevice, build_atmosphere_luts_for, sample_aerial,
    transmitted_sun,
};

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

fn lum(c: [f32; 3]) -> f64 {
    0.2126 * f64::from(c[0]) + 0.7152 * f64::from(c[1]) + 0.0722 * f64::from(c[2])
}

fn henyey_greenstein(cos_t: f64, g: f64) -> f64 {
    let d = (1.0 + g * g - 2.0 * g * cos_t).max(1e-6);
    (1.0 - g * g) / (4.0 * std::f64::consts::PI * d * d.sqrt())
}

/// First-order in-scatter of the air (luminance) along the level view ray `+Z` from `eye_y`
/// to `wall_m`, lit by the sun, the sky table above the horizon and the Lambert ground below
/// it (lit by the sun and the table's sky): the light the path tracer's medium vertices see,
/// integrated by quadrature over the sphere. Higher orders inside the air are left out
/// (optical depth ≈ 0.03 at 1 km).
fn first_order(
    planet: &AtmosphereParameters,
    luts: &regia_render::AtmosphereLuts,
    sun: ([f32; 3], [f32; 3]),
    eye_y: f32,
    wall_m: f32,
) -> f64 {
    let (toward, e_sun) = sun;
    let amb = regia_render::ambient_irradiance_table(luts);
    let view = [0.0f64, 0.0, 1.0];
    let n = 8192usize;
    let golden = std::f64::consts::PI * (3.0 - 5.0f64.sqrt());
    let mut out = [0.0f64; 3];
    for c in 0..3 {
        let rho_m = f64::from(planet.mie_scattering[c]) / 1000.0
            * (-f64::from(eye_y) / (f64::from(planet.mie_scale_height_km) * 1000.0)).exp();
        let rho_r = f64::from(planet.rayleigh_scattering[c]) / 1000.0
            * (-f64::from(eye_y) / (f64::from(planet.rayleigh_scale_height_km) * 1000.0)).exp();
        let ext = f64::from(planet.mie_extinction[c]) / 1000.0
            * (-f64::from(eye_y) / (f64::from(planet.mie_scale_height_km) * 1000.0)).exp()
            + rho_r;
        let g = f64::from(planet.mie_g[c]);
        let phase = |cos_t: f64| {
            let ray = 3.0 / (16.0 * std::f64::consts::PI) * (1.0 + cos_t * cos_t);
            (rho_m * henyey_greenstein(cos_t, g) + rho_r * ray) / (rho_m + rho_r)
        };
        let cos_sun = f64::from(toward[2]);
        let mut li = phase(cos_sun) * f64::from(e_sun[c]);
        let ground = f64::from(planet.ground_albedo[c])
            * (f64::from(e_sun[c]) * f64::from(toward[1]).max(0.0) / std::f64::consts::PI
                + f64::from(amb[c]));
        let mut sphere = 0.0;
        for i in 0..n {
            let y = 1.0 - (2.0 * i as f64 + 1.0) / n as f64;
            let r = (1.0 - y * y).max(0.0).sqrt();
            let a = golden * i as f64;
            let w = [r * a.cos(), y, r * a.sin()];
            let l = if y > 0.0 {
                f64::from(regia_render::sample_sky_table(luts, [w[0] as f32, w[1] as f32, w[2] as f32])[c])
            } else {
                ground
            };
            sphere += phase(w[0] * view[0] + w[1] * view[1] + w[2] * view[2]) * l;
        }
        li += sphere * 4.0 * std::f64::consts::PI / n as f64;
        let d = f64::from(wall_m);
        out[c] = (rho_m + rho_r) * li * (1.0 - (-ext * d).exp()) / ext;
    }
    0.2126 * out[0] + 0.7152 * out[1] + 0.0722 * out[2]
}

/// Mean PT in-scatter in front of a black wall `wall_m` out, the table's and the first-order
/// quadrature's, for a sun at `elevation` and `azimuth` from the view direction (radians).
fn in_scatter(planet: &AtmosphereParameters, elevation: f32, azimuth: f32, wall_m: f32) -> (f64, f64, f64) {
    let eye_y = 20.0f32;
    let toward = [
        elevation.cos() * azimuth.sin(),
        elevation.sin(),
        elevation.cos() * azimuth.cos(),
    ];
    let luts = build_atmosphere_luts_for(planet, toward, eye_y);
    // A narrow lens looking level along +Z: every ray meets the wall before the ground.
    let cam = PtCamera::pinhole([0.0, eye_y, 0.0], [0.0, eye_y, 1.0], 0.02, 8, 8);
    let mut scene = PtScene::new(cam);
    let ground = scene.add_mesh(scenes::quad([
        [-40_000.0, 0.0, -40_000.0],
        [-40_000.0, 0.0, 40_000.0],
        [40_000.0, 0.0, 40_000.0],
        [40_000.0, 0.0, -40_000.0],
    ]));
    // Just wider than the view, so its shadow covers almost none of the air in front.
    let half = 0.015 * wall_m + 1.0;
    let wall = scene.add_mesh(scenes::quad([
        [-half, eye_y - half, wall_m],
        [half, eye_y - half, wall_m],
        [half, eye_y + half, wall_m],
        [-half, eye_y + half, wall_m],
    ]));
    let g = planet.ground_albedo;
    let ground_mat = scene.add_material(PtMaterial::lambert(g));
    let black = scene.add_material(PtMaterial::lambert([0.0; 3]));
    scene.add_instance(ground, ground_mat, Mat4::IDENTITY);
    scene.add_instance(wall, black, Mat4::IDENTITY);
    let (color, illuminance) = transmitted_sun(&luts, planet);
    scene.sun = Some(PtSun {
        toward,
        color,
        illuminance,
        angular_radius: planet.sun_angular_radius,
    });
    scene.sky = PtSky::from_atmosphere(&luts);
    let outdoor = OutdoorParams {
        planet: *planet,
        ..OutdoorParams::default()
    };
    scene.media = media_from_outdoor(&outdoor);
    let settings = PtSettings {
        spp: 1024,
        max_bounces: 12,
        seed: 5,
        rr_start: 3,
        jitter: true,
        samples_per_submit: 128,
    };
    let img = tracer().render(&scene, &settings).expect("render");
    let pt = img.beauty.iter().map(|c| lum(*c)).sum::<f64>() / img.beauty.len() as f64;
    let (table, _) = sample_aerial(&luts, [0.0, 0.0, 1.0], wall_m);
    let e_sun = color.map(|k| k * illuminance);
    let reference = first_order(planet, &luts, (toward, e_sun), eye_y, wall_m);
    (pt, lum(table), reference)
}

#[test]
fn mars_air_in_scatter_matches_the_aerial_table_on_open_ground() {
    let planet = AtmosphereParameters::mars();
    let mut worst = 0.0f64;
    let mut worst_first = 0.0f64;
    for (elevation, azimuth) in [(35.0f32, 75.0f32), (35.0, 150.0), (20.0, 30.0)] {
        for wall in [550.0f32, 1000.0] {
            let (pt, table, first) = in_scatter(&planet, elevation.to_radians(), azimuth.to_radians(), wall);
            let ratio = table / pt.max(1e-9);
            eprintln!(
                "sun {elevation}° az {azimuth}°, wall {wall} m: PT {pt:.2} cd/m², table {table:.2} (table/PT {ratio:.3}), first-order quadrature {first:.2} (/PT {:.3})",
                first / pt.max(1e-9)
            );
            worst = worst.max((ratio - 1.0).abs());
            worst_first = worst_first.max((first / pt.max(1e-9) - 1.0).abs());
        }
    }
    eprintln!("worst |table/PT - 1| = {worst:.3}, worst |quadrature/PT - 1| = {worst_first:.3}");
    // Measured on lavapipe and an RTX 5080: table/PT 0.83–1.11 (the 32×16 table smears the
    // forward lobe), quadrature/PT 1.05–1.10 (it leaves out the air's higher orders).
    assert!(
        worst < 0.25,
        "aerial table vs path tracer: worst |ratio - 1| {worst}"
    );
    assert!(
        worst_first < 0.25,
        "first-order quadrature vs path tracer: worst |ratio - 1| {worst_first}"
    );
}
