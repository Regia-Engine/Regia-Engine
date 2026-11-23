// SPDX-License-Identifier: MPL-2.0
//! The path tracer's cloud field (ADR-2127): delta and ratio tracking against closed forms and an
//! independent transport, and the raster sky's density against the raster march. Needs a GPU
//! adapter (lavapipe is enough).
//!
//! - Uniform shells have known answers: the unscattered view (delta tracking), the sun's beam
//!   (ratio tracking), single scattering in closed form, all orders against the 1D reference.
//! - A white furnace holds for the real density: a lossless medium in a uniform sky leaves every
//!   pixel at the sky's radiance, whatever the density does. This fails on a wrong majorant, a
//!   wrong phase pdf, a lost weight or a shadow ray that disagrees with the free flights.
//! - The same texels: the traced transmittance and the raster march's alpha agree block by block,
//!   and the traced sun beam and the raster's cloud transmittance map too.

use std::sync::OnceLock;

use regia_pt::cloud::{PtCloudLayer, slab_transport_reference};
use regia_pt::cloud_compare::{
    CloudCase, CloudTraceSize, RasterMarch, compare_ground, compare_view, measure_view, physical_field,
    raster_parameters_field, raster_view, standard_cases, traced_view,
};
use regia_pt::{
    PathTracer, PtCamera, PtCloudDensity, PtCloudField, PtCloudPhase, PtImage, PtMaterial, PtScene,
    PtSettings, PtSky, PtSun, scenes,
};
use regia_render::{CloudGenus, CloudSettings, RenderDevice};

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

fn mean_of(values: &[[f32; 3]]) -> f64 {
    values.iter().map(|c| lum(*c)).sum::<f64>() / values.len().max(1) as f64
}

/// A planet large enough that the shells are flat over the scene: 63 600 km. At the ground's edge
/// (150 km) a shell has dropped 177 m, at the ADR-2127 layer about 7 % of its thickness.
const FLAT_PLANET_M: f32 = 6.36e7;
const BASE_M: f32 = 1500.0;
const TOP_M: f32 = 4000.0;

/// A horizontally uniform field of vertical optical depth `tau`.
fn uniform_field(tau: f32) -> PtCloudField {
    let settings = CloudSettings {
        altitude_bottom: BASE_M,
        altitude_top: TOP_M,
        density: tau / (TOP_M - BASE_M),
        ..CloudSettings::coverage_layer(1.0)
    };
    PtCloudField {
        density: PtCloudDensity::Uniform(1.0),
        ..PtCloudField::from_settings(settings, FLAT_PLANET_M)
    }
}

fn settings(spp: u32, max_bounces: u32) -> PtSettings {
    PtSettings {
        spp,
        max_bounces,
        seed: 5,
        rr_start: max_bounces,
        jitter: true,
        samples_per_submit: 16,
    }
}

/// A narrow camera looking up `elevation` from the horizon, azimuth +Z.
fn up_camera(elevation: f32, size: u32) -> PtCamera {
    let eye = [0.0, 2.0, 0.0];
    let target = [0.0, 2.0 + elevation.sin(), elevation.cos()];
    PtCamera::pinhole(eye, target, 0.01, size, size)
}

fn sun_at(elevation: f32) -> PtSun {
    PtSun {
        toward: [elevation.cos(), elevation.sin(), 0.0],
        color: [1.0; 3],
        illuminance: 100_000.0,
        angular_radius: 0.0047,
    }
}

/// One definition: both shaders import the density and neither keeps its own copy of its steps.
#[test]
fn the_density_has_one_definition() {
    let raster = include_str!("../../regia_render/shaders/clouds.wgsl");
    let traced = include_str!("../shaders/pt.wgsl");
    assert!(raster.contains("#import regia::cloud_density"));
    assert!(traced.contains("#import regia::cloud_density"));
    for step in ["fn type_gradient", "fn remap", "let fbm = s.g", "let dfbm = d.r"] {
        assert!(!raster.contains(step), "clouds.wgsl keeps `{step}`");
        assert!(!traced.contains(step), "pt.wgsl keeps `{step}`");
    }
}

/// Delta tracking: the view through a uniform shell keeps `exp(−τ / sin(elevation))` of the sky.
#[test]
fn the_unscattered_view_through_a_uniform_shell_is_beer_lambert() {
    let elevation = 50f32.to_radians();
    let sky_nit = 1000.0;
    for tau in [0.3_f32, 1.0, 2.5] {
        let mut scene = PtScene::new(up_camera(elevation, 8));
        scene.sky = PtSky::Uniform([sky_nit; 3]);
        scene.clouds = Some(uniform_field(tau));
        let img = tracer().render(&scene, &settings(4096, 1)).expect("trace");
        let t = mean_of(&img.sky) / f64::from(sky_nit);
        let beer = f64::from((-tau / elevation.sin()).exp());
        assert!(
            (t - beer).abs() < 0.015,
            "tau {tau}: traced view {t}, Beer–Lambert {beer}"
        );
    }
}

/// A camera over a lit lambert plane, the plane wide enough for the sun's shadow.
fn ground_scene(sun: PtSun, albedo: f32, field: Option<PtCloudField>) -> PtScene {
    let elevation = sun.toward[1].asin();
    let mut scene = scenes::sun_plane(
        PtMaterial::lambert([albedo; 3]),
        elevation,
        sun.illuminance,
        sun.angular_radius,
        32,
    );
    let h = 150_000.0;
    if let Some(mesh) = scene.meshes.first_mut() {
        *mesh = scenes::quad([[-h, 0.0, -h], [-h, 0.0, h], [h, 0.0, h], [h, 0.0, -h]]);
    }
    scene.clouds = field;
    scene
}

/// Ratio tracking: the sun's direct light on the ground keeps `exp(−τ / sin(elevation))`.
#[test]
fn the_suns_direct_beam_through_a_uniform_shell_is_beer_lambert() {
    let elevation = 60f32.to_radians();
    let sun = sun_at(elevation);
    let clear = mean_of(
        &tracer()
            .render(&ground_scene(sun, 0.5, None), &settings(16, 1))
            .expect("trace")
            .beauty,
    );
    for tau in [0.5_f32, 1.5, 4.0] {
        let scene = ground_scene(sun, 0.5, Some(uniform_field(tau)));
        let img = tracer().render(&scene, &settings(1024, 1)).expect("trace");
        let t = mean_of(&img.beauty) / clear;
        let beer = f64::from((-tau / elevation.sin()).exp());
        assert!(
            (t - beer).abs() < 0.015 + 0.03 * beer,
            "tau {tau}: traced beam {t}, Beer–Lambert {beer}"
        );
    }
}

fn henyey_greenstein(cos_t: f64, g: f64) -> f64 {
    let d = 1.0 + g * g - 2.0 * g * cos_t;
    (1.0 - g * g) / (4.0 * std::f64::consts::PI * d * d.sqrt())
}

/// One path vertex is single scattering. Seen straight up through a uniform slab, in closed form:
/// `E · p(θ) · σ · ∫₀ᴴ e^(−σ s) e^(−σ (H − s) / μ) ds`.
#[test]
fn single_scattering_of_a_uniform_shell_has_its_closed_form() {
    let elevation = 40f32.to_radians();
    let sun = sun_at(elevation);
    let mu = f64::from(elevation.sin());
    let h = f64::from(TOP_M - BASE_M);
    for (tau, phase) in [
        (0.4_f32, PtCloudPhase::DROPLET),
        (1.5, PtCloudPhase::DROPLET),
        (1.5, PtCloudPhase::RASTER_DUAL_LOBE),
    ] {
        let mut scene = PtScene::new(up_camera(89.9f32.to_radians(), 8));
        scene.sun = Some(sun);
        let mut field = uniform_field(tau);
        field.phase = phase;
        scene.clouds = Some(field);
        let img = tracer().render(&scene, &settings(16_384, 1)).expect("trace");
        let traced = mean_of(&img.volumetric);
        let sigma = f64::from(tau) / h;
        // The sun's light travels down its ray, the eye's light straight down: cos θ = μ.
        let cos_t = mu;
        let p = f64::from(phase.forward_weight) * henyey_greenstein(cos_t, f64::from(phase.forward_g))
            + (1.0 - f64::from(phase.forward_weight)) * henyey_greenstein(cos_t, f64::from(phase.back_g));
        let a = sigma * (1.0 - 1.0 / mu);
        let closed = 100_000.0 * p * sigma * (-sigma * h / mu).exp() * ((-a * h).exp() - 1.0) / -a;
        assert!(
            (traced / closed - 1.0).abs() < 0.05,
            "tau {tau} {phase:?}: traced {traced}, closed form {closed}"
        );
    }
}

/// All orders: the traced shell's ground light against the transport reference (an independent
/// 1D Monte Carlo) and against the same slab through the tracer's analytic medium layer.
#[test]
fn every_scattering_order_of_a_uniform_shell_matches_the_slab() {
    let sun = sun_at(50f32.to_radians());
    let sun_y = sun.toward[1];
    let albedo = regia_pt::cloud::VALIDATION_GROUND_ALBEDO;
    let clear = mean_of(
        &tracer()
            .render(&ground_scene(sun, albedo, None), &settings(64, 32))
            .expect("trace")
            .beauty,
    );
    for tau in [1.0_f32, 4.0, 10.0] {
        let scene = ground_scene(sun, albedo, Some(uniform_field(tau)));
        let img = tracer().render(&scene, &settings(128, 256)).expect("trace");
        let traced = mean_of(&img.beauty) / clear;
        let layer = PtCloudLayer::overcast(BASE_M, TOP_M, tau);
        let reference = slab_transport_reference(&layer, Some(sun_y), albedo, 40_000, 1);
        let mut analytic = ground_scene(sun, albedo, None);
        layer.add_to(&mut analytic).expect("layer");
        let slab = mean_of(
            &tracer()
                .render(&analytic, &settings(128, 256))
                .expect("trace")
                .beauty,
        ) / clear;
        assert!(
            (traced - slab).abs() < 0.06,
            "tau {tau}: field {traced}, analytic slab {slab}"
        );
        assert!(
            (traced - reference).abs() < 0.08,
            "tau {tau}: field {traced}, transport reference {reference}"
        );
    }
}

fn furnace(field: PtCloudField, sky: f32) -> PtImage {
    let eye = [0.0, 2.0, 0.0];
    let camera = PtCamera::pinhole(eye, [0.0, 1000.0, 1000.0], 55f32.to_radians(), 32, 32);
    let mut scene = PtScene::new(camera);
    scene.sky = PtSky::Uniform([sky; 3]);
    scene.clouds = Some(field);
    tracer().render(&scene, &settings(48, 128)).expect("trace")
}

/// White furnace: a lossless medium in a uniform sky leaves every pixel at the sky's radiance. Real
/// density, both phases: anything biased in the tracking, the phase pdf or the weights moves it.
#[test]
fn the_real_density_passes_the_white_furnace() {
    let sky = 1000.0_f32;
    // Fair cumulus rather than the dense heaps: at 128 vertices a cumulus core still holds about
    // 4 % of the light (measured: 0.955 at 128, 0.993 at 512, 0.997 at 1024).
    let genus = CloudSettings {
        wind_m_per_s: [0.0, 0.0],
        ..CloudSettings::default().with_genus(CloudGenus::FairCumulus)
    };
    for (name, settings, phase) in [
        (
            "default",
            CloudSettings::coverage_layer(0.45),
            PtCloudPhase::DROPLET,
        ),
        ("fair cumulus", genus, PtCloudPhase::RASTER_DUAL_LOBE),
    ] {
        let field = PtCloudField {
            phase,
            ..PtCloudField::from_settings(settings, 6_360_000.0)
        };
        let img = furnace(field, sky);
        let m = mean_of(&img.beauty) / f64::from(sky);
        // Some rays did scatter, so this is not an empty sky.
        let scattered = mean_of(&img.volumetric) / f64::from(sky);
        println!("{name}: furnace mean {m}, scattered part {scattered}");
        assert!(scattered > 0.05, "{name}: no cloud in view ({scattered})");
        assert!((m - 1.0).abs() < 0.02, "{name}: furnace mean {m}");
    }
}

/// A case's clouds with the march as fine as it goes: the block transmittance is then the density's,
/// not the step size's. Same weather map, same noise, same curl, so the blocks must agree. The
/// transmittance is the first free flight's, so one path vertex is enough.
fn fine_transmittance_agreement(case: &CloudCase) -> regia_pt::cloud_compare::CloudViewMeasure {
    let light = case.light();
    let size = CloudTraceSize::TEST;
    let raster = raster_view(tracer().device(), case, &light, &RasterMarch::FINEST, &size).expect("raster");
    let (traced, clear) =
        traced_view(tracer(), case, &light, physical_field(case, &light), &size, 1).expect("trace");
    measure_view(&raster, &traced, &clear)
}

/// The one definition holds: the traced view transmittance and the raster march's alpha are the
/// same field, block by block, for the default layer and a genus with breakup and a different
/// erosion.
#[test]
fn the_traced_transmittance_is_the_raster_density() {
    let cases = standard_cases();
    let default_layer = cases[0];
    let cumulus = cases
        .iter()
        .find(|c| c.name == "cumulus")
        .copied()
        .expect("a cumulus case");
    for case in [default_layer, cumulus] {
        let m = fine_transmittance_agreement(&case);
        println!("{}: {m:?}", case.name);
        assert!(m.cloudy_blocks > 0, "{}: no cloud in view", case.name);
        assert!(
            (m.raster_transmittance - m.pt_transmittance).abs() < 0.03,
            "{}: mean transmittance raster {} traced {}",
            case.name,
            m.raster_transmittance,
            m.pt_transmittance
        );
        assert!(
            m.transmittance_block_correlation > 0.97,
            "{}: block correlation {}",
            case.name,
            m.transmittance_block_correlation
        );
        assert!(
            m.transmittance_block_rms < 0.05,
            "{}: block rms {}",
            case.name,
            m.transmittance_block_rms
        );
    }
}

/// The traced sun beam (one vertex on the ground: ratio tracking of the sun's shadow ray) and the
/// raster's cloud transmittance map over the same ground.
#[test]
fn the_traced_sun_beam_is_the_raster_cloud_map() {
    let case = standard_cases()[0];
    let light = case.light();
    let size = CloudTraceSize {
        width: 64,
        height: 64,
        spp: 8,
        max_bounces: 32,
        seed: 3,
        raster_frames: 1,
    };
    let m = compare_ground(tracer(), &case, physical_field(&case, &light), &size).expect("ground");
    println!("{}: {m:?}", case.name);
    assert!(
        (m.raster_sun - m.pt_sun_direct).abs() < 0.05,
        "mean sun transmittance: map {}, traced {}",
        m.raster_sun,
        m.pt_sun_direct
    );
    assert!(
        m.sun_direct_block_correlation > 0.9,
        "block correlation {}",
        m.sun_direct_block_correlation
    );
    // Scattered sunlight only adds to the unscattered beam.
    assert!(m.pt_sun_all > m.pt_sun_direct - 0.02);
}

/// The measurement itself on a small trace: printed for the log, and bounded only by what must
/// hold whatever the raster's look is (ADR-2127 holds the numbers of the full run).
#[test]
fn the_comparison_returns_finite_numbers_at_a_standard_sky() {
    let case = standard_cases()[0];
    let light = case.light();
    let size = CloudTraceSize {
        spp: 24,
        max_bounces: 64,
        ..CloudTraceSize::TEST
    };
    for (label, field) in [
        ("physical", physical_field(&case, &light)),
        ("raster parameters", raster_parameters_field(&case, &light)),
    ] {
        let m = compare_view(tracer(), &case, &RasterMarch::DESKTOP_HIGH, field, &size).expect("compare");
        println!("{label}: {m:?}");
        assert!(m.raster_radiance.is_finite() && m.raster_radiance > 0.0);
        assert!(m.pt_radiance.is_finite() && m.pt_radiance > 0.0);
        assert!(m.radiance_log2.abs() < 8.0, "{label}: {} stops", m.radiance_log2);
        assert!(m.radiance_block_correlation > 0.5, "{label}: same clouds in view");
    }
}

/// The same scene and seed trace the same image, bit for bit.
#[test]
fn a_cloud_trace_repeats() {
    let case = standard_cases()[0];
    let light = case.light();
    let eye = [0.0, 3.0, 0.0];
    let camera = PtCamera::pinhole(eye, [0.0, 1000.0, 1000.0], 55f32.to_radians(), 16, 16);
    let run = || {
        let scene =
            regia_pt::cloud_compare::compare_scene(camera, &light, Some(physical_field(&case, &light)), true);
        tracer().render(&scene, &settings(8, 32)).expect("trace")
    };
    assert!(run().beauty == run().beauty);
}

/// A field with no density draws no random numbers and adds no light: the trace is bit for bit the
/// trace without a field, with a medium in the scene as well.
#[test]
fn an_empty_field_leaves_a_medium_scene_unchanged() {
    let sun = sun_at(50f32.to_radians());
    let layer = PtCloudLayer::overcast(BASE_M, TOP_M, 3.0);
    let mut a = ground_scene(sun, 0.5, None);
    layer.add_to(&mut a).expect("layer");
    let mut b = a.clone();
    b.clouds = Some(uniform_field(0.0));
    let run = |s: &PtScene| tracer().render(s, &settings(16, 8)).expect("trace").beauty;
    assert!(run(&a) == run(&b), "an empty field moved the image");
}
