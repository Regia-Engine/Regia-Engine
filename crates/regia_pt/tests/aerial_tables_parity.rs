// SPDX-License-Identifier: MPL-2.0
//! Ground haze at 1, 5 and 20 km, raster `aerial_from_tables` off and on, against the path
//! tracer (ADR-3294). The scene is `aerial_parity.rs`'s: a camera 20 m up looking level along +Z
//! at a black wall, so every pixel is the air's in-scatter alone, over open flat ground on Earth.
//! Heavy rain adds a homogeneous scattering slab (σ = 3.912 / 1000 m, 1 km visibility, isotropic,
//! albedo 1) up to the deck's base, which the tracer integrates with the sky and the sun.
//!
//! The raster values are the CPU mirrors of the shader terms (`regia_render::horizon_seam`): off,
//! the forward pass's analytic rain in-scatter then the aerial table; on, the aerial table then
//! the weather veil toward the rain's in-scatter (`rain_inscatter`: the sphere average of the
//! horizon row, the lit ground and the sun, no deck here). Numbers and FLIP go to stderr for the report;
//! the asserts hold what the design promises (clear air is unchanged inside 32 km) and what was
//! measured (in heavy rain the tables' term is nearer the tracer than the analytic one).

use std::sync::OnceLock;

use regia_core::glam::Mat4;
use regia_pt::extract::media_from_outdoor;
use regia_pt::scenes;
use regia_pt::{PathTracer, PtCamera, PtMaterial, PtMediumLayer, PtPhase, PtScene, PtSettings, PtSky, PtSun};
use regia_render::horizon_seam::{forward_weather_inscatter, luminance, rain_inscatter, table_air};
use regia_render::{
    AtmosphereParameters, FogSettings, OutdoorParams, RenderDevice, TableAir, build_atmosphere_luts_for,
    transmitted_sun,
};

const RAIN_SIGMA: f32 = 3.912e-3;
const EYE_Y: f32 = 20.0;
const SIZE: u32 = 8;

fn tracer() -> &'static PathTracer {
    static T: OnceLock<PathTracer> = OnceLock::new();
    T.get_or_init(|| {
        let device = RenderDevice::headless().expect("headless device");
        PathTracer::new(&device).expect("compile the path tracer")
    })
}

/// One wall distance: the tracer's mean in-scatter and the raster's off and on, all cd/m².
struct Haze {
    pt: [f32; 3],
    off: [f32; 3],
    on: [f32; 3],
}

fn haze(sigma: f32, wall_m: f32) -> Haze {
    let planet = AtmosphereParameters::earth();
    let (elevation, azimuth) = (35f32.to_radians(), 75f32.to_radians());
    let toward = [
        elevation.cos() * azimuth.sin(),
        elevation.sin(),
        elevation.cos() * azimuth.cos(),
    ];
    let luts = build_atmosphere_luts_for(&planet, toward, EYE_Y);
    let cam = PtCamera::pinhole([0.0, EYE_Y, 0.0], [0.0, EYE_Y, 1.0], 0.02, SIZE, SIZE);
    let mut scene = PtScene::new(cam);
    let ground = scene.add_mesh(scenes::quad([
        [-40_000.0, 0.0, -40_000.0],
        [-40_000.0, 0.0, 40_000.0],
        [40_000.0, 0.0, 40_000.0],
        [40_000.0, 0.0, -40_000.0],
    ]));
    let half = 0.015 * wall_m + 1.0;
    let wall = scene.add_mesh(scenes::quad([
        [-half, EYE_Y - half, wall_m],
        [half, EYE_Y - half, wall_m],
        [half, EYE_Y + half, wall_m],
        [-half, EYE_Y + half, wall_m],
    ]));
    let ground_mat = scene.add_material(PtMaterial::lambert(planet.ground_albedo));
    let black = scene.add_material(PtMaterial::lambert([0.0; 3]));
    scene.add_instance(ground, ground_mat, Mat4::IDENTITY);
    scene.add_instance(wall, black, Mat4::IDENTITY);
    let (color, illuminance) = transmitted_sun(&luts, &planet);
    scene.sun = Some(PtSun {
        toward,
        color,
        illuminance,
        angular_radius: planet.sun_angular_radius,
    });
    scene.sky = PtSky::from_atmosphere(&luts);
    scene.media = media_from_outdoor(&OutdoorParams {
        planet,
        ..OutdoorParams::default()
    });
    if sigma > 0.0 {
        scene.media.layers.push(PtMediumLayer {
            scattering: [sigma; 3],
            absorption: [0.0; 3],
            reference_height: 0.0,
            falloff: 0.0,
            min_height: -1.0e9,
            max_height: 1200.0,
            phase: PtPhase::HenyeyGreenstein([0.0; 3]),
            aerial: true,
        });
    }
    let settings = PtSettings {
        spp: 512,
        max_bounces: 8,
        seed: 11,
        rr_start: 3,
        jitter: true,
        samples_per_submit: 128,
    };
    let img = tracer().render(&scene, &settings).expect("render");
    let n = img.beauty.len().max(1) as f32;
    let pt = img.beauty.iter().fold([0.0f32; 3], |a, c| {
        [a[0] + c[0] / n, a[1] + c[1] / n, a[2] + c[2] / n]
    });

    let view = [0.0, 0.0, 1.0];
    let tw = (-sigma * wall_m).exp();
    // Off: the forward pass's rain over the black wall, then the aerial table.
    let fog = FogSettings {
        sky_inscatter: true,
        ..FogSettings::default()
    };
    let rain = forward_weather_inscatter(view, toward, color.map(|c| c * illuminance), &fog, 1.0);
    let (air, t) = table_air(&luts, view, wall_m, TableAir::Off);
    let off = std::array::from_fn(|c| rain[c] * (1.0 - tw) * t + air[c]);
    // On: the aerial table, then the veil toward the rain's in-scatter (no deck).
    let (air, _) = table_air(&luts, view, wall_m, TableAir::Full);
    let veil = rain_inscatter(&luts, view, 0.0);
    let on = std::array::from_fn(|c| air[c] * tw + veil[c] * (1.0 - tw));
    Haze { pt, off, on }
}

fn encode(c: [f32; 3], exposure: f32) -> [f32; 3] {
    c.map(|v| {
        let x = (v * exposure).max(0.0);
        let m = x / (1.0 + x);
        if m <= 0.003_130_8 {
            12.92 * m
        } else {
            1.055 * m.powf(1.0 / 2.4) - 0.055
        }
    })
}

/// FLIP of a flat `test` patch against a flat `reference` patch (8×8 at 30 pixels per degree),
/// both at the reference's exposure (its luminance maps to 0.5 before the curve).
fn flip(reference: [f32; 3], test: [f32; 3]) -> f32 {
    let exposure = 1.0 / luminance(reference).max(1e-6);
    let n = (SIZE * SIZE) as usize;
    let r = vec![encode(reference, exposure); n];
    let t = vec![encode(test, exposure); n];
    regia_render::compare::flip(&r, &t, SIZE, SIZE, 30.0).mean
}

#[test]
fn ground_haze_off_and_on_against_the_path_tracer() {
    for (name, sigma) in [("clear", 0.0f32), ("heavy_rain", RAIN_SIGMA)] {
        for wall in [1000.0f32, 5000.0, 20_000.0] {
            let h = haze(sigma, wall);
            let (pt, off, on) = (luminance(h.pt), luminance(h.off), luminance(h.on));
            eprintln!(
                "{name} {wall} m: PT {pt:.1} cd/m², off {off:.1} (/PT {:.3}, FLIP {:.4}), on {on:.1} (/PT {:.3}, FLIP {:.4})",
                off / pt.max(1e-6),
                flip(h.pt, h.off),
                on / pt.max(1e-6),
                flip(h.pt, h.on),
            );
            assert!(pt.is_finite() && off.is_finite() && on.is_finite());
            if sigma == 0.0 {
                // Clear air inside the tables' range: the switch changes nothing.
                assert_eq!(h.off, h.on, "clear {wall} m");
            } else {
                // Rain: the tables' in-scatter is nearer the tracer than the analytic one (measured
                // on lavapipe: FLIP 0.25 on, 0.42-0.45 off).
                assert!(flip(h.pt, h.on) < flip(h.pt, h.off), "rain {wall} m");
            }
        }
    }
}
