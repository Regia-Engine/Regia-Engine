// SPDX-License-Identifier: MPL-2.0
//! Rain and snow on the VFX harness (spec 99 § 11): nothing falling draws nothing, what falls
//! draws a bounded amount, the same clock draws the same bytes, a cut shows a full field, and a
//! change of kind moves the picture a little at a time.

use regia_core::math::Vec3;
use regia_render::{QualityTier, RenderDevice};
use regia_test_utils::serial_gpu::{SerialGpu, serial_device};
use regia_vfx::gpu::VfxLighting;
use regia_vfx::offscreen::VfxHarness;
use regia_vfx::{
    PrecipFrame, PrecipIntegrator, PrecipResolution, PrecipitationRates, PrecipitationSettings, VfxCamera,
    VfxProfile, Weather, WeatherConfig, WeatherState,
};

// Large enough for thin streaks: a 2.5 mm drop covers about half a pixel at 5 m here.
const W: u32 = 960;
const H: u32 = 540;
const DT: f32 = 1.0 / 60.0;

/// The binary's one shared device, one test at a time: per-test devices on parallel threads hung
/// the NVIDIA driver (RTX 5090 GPU pre-gate, ADR-7483).
/// It is also the loader-race fix of ADR-7557 (the ambient_frame pattern): each test used to open
/// and drop its own device while other test threads named objects, the Vulkan loader list race of
/// ADR-2113, in the build server's journal for this binary since 11-08. Every test still builds its own
/// harness on it.
fn device() -> SerialGpu {
    match serial_device() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A second `device()` call opens no instance and no device (`loader_list_lock_count` counts both),
/// so the binary creates its one device once, whichever test asks first. The first turn is given
/// back before the second is taken: a second `serial_device` on the same thread while the first
/// lives waits forever on a GPU (ADR-7483).
#[test]
fn every_test_shares_the_binary_device() {
    let first = device();
    let first_id = first.device_id();
    drop(first);
    let before = regia_render::loader_list_lock_count();
    let second = device();
    assert_eq!(
        regia_render::loader_list_lock_count(),
        before,
        "a second device() call created an instance or a device"
    );
    assert_eq!(first_id, second.device_id());
}

fn camera(eye: Vec3) -> VfxCamera {
    VfxCamera::looking_at(eye, eye + Vec3::new(0.0, -0.1, -1.0))
}

fn settings() -> PrecipitationSettings {
    PrecipitationSettings {
        enabled: true,
        ..PrecipitationSettings::default()
    }
}

fn frame(
    integrator: &PrecipIntegrator,
    rates: &PrecipitationRates,
    eye: Vec3,
    wind: [f32; 3],
) -> PrecipFrame {
    integrator.frame(
        [f64::from(eye.x), f64::from(eye.y), f64::from(eye.z)],
        [0.0; 3],
        [0.0, -1.0],
        0.0,
        rates,
        &WeatherConfig::default(),
        &settings(),
        wind,
        QualityTier::Low,
    )
}

/// [`frame`] with the drops drawn at `resolution` (`render.set_precipitation { resolution }`).
fn frame_at(
    integrator: &PrecipIntegrator,
    rates: &PrecipitationRates,
    eye: Vec3,
    wind: [f32; 3],
    resolution: PrecipResolution,
) -> PrecipFrame {
    integrator.frame(
        [f64::from(eye.x), f64::from(eye.y), f64::from(eye.z)],
        [0.0; 3],
        [0.0, -1.0],
        0.0,
        rates,
        &WeatherConfig::default(),
        &PrecipitationSettings {
            resolution: Some(resolution),
            ..settings()
        },
        wind,
        QualityTier::Low,
    )
}

fn render(device: &RenderDevice, frame: Option<PrecipFrame>, eye: Vec3) -> Vec<u8> {
    let mut harness = VfxHarness::new(device, VfxProfile::default()).expect("harness");
    // A bright sky over the mid-grey ground, as a day frame has: what a drop refracts.
    harness.set_lighting(VfxLighting {
        ambient: 1.0,
        ..VfxLighting::default()
    });
    harness.set_precipitation(device, frame);
    harness
        .render(device, &camera(eye), W, H, 0.0)
        .expect("frame")
        .pixels
}

/// Share of pixels whose largest channel moved by more than 2 of 255.
fn coverage(a: &[u8], b: &[u8]) -> f32 {
    let (ap, bp) = (a.as_chunks::<4>().0, b.as_chunks::<4>().0);
    let changed = ap
        .iter()
        .zip(bp)
        .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > 2))
        .count();
    changed as f32 / ap.len() as f32
}

fn mean(pixels: &[u8]) -> f32 {
    let values = pixels.as_chunks::<4>().0;
    values
        .iter()
        .map(|p| (f32::from(p[0]) + f32::from(p[1]) + f32::from(p[2])) / 3.0)
        .sum::<f32>()
        / values.len() as f32
}

/// Precipitation 0 records no draw: the frame's bytes equal a run without the module.
#[test]
fn nothing_falling_draws_nothing() {
    let device = device();
    let eye = Vec3::new(0.0, 1.7, 8.0);
    let config = WeatherConfig::default();
    let clear = WeatherState::CLEAR.rates(&config);
    let plain = render(&device, None, eye);
    let off = render(
        &device,
        Some(frame(&PrecipIntegrator::default(), &clear, eye, [0.0; 3])),
        eye,
    );
    assert_eq!(plain, off);
}

/// Storm rain changes the frame without blowing it out.
#[test]
fn storm_rain_draws_thin_streaks() {
    let device = device();
    let eye = Vec3::new(0.0, 1.7, 8.0);
    let config = WeatherConfig::default();
    let storm = WeatherState::STORM.rates(&config);
    let plain = render(&device, None, eye);
    let rain = render(
        &device,
        Some(frame(&PrecipIntegrator::default(), &storm, eye, [4.0, 0.0, 1.0])),
        eye,
    );
    let share = coverage(&plain, &rain);
    assert!((0.005..0.7).contains(&share), "rain covers {share}");
    let brightest = rain
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] == 255 && p[1] == 255)
        .count();
    let base = plain
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] == 255 && p[1] == 255)
        .count();
    assert!(
        brightest <= base + (W * H / 50) as usize,
        "blown out: {brightest} over {base}"
    );
}

/// The harness draws rain at the reduced resolutions too (corr-vfx L1): it records the reduced
/// pass before the pass that composites it, as the runtime layer does. Without that, `Half` and
/// `Quarter` composited an empty target and the frame matched a run without rain.
#[test]
fn storm_rain_draws_at_reduced_resolutions() {
    let device = device();
    let eye = Vec3::new(0.0, 1.7, 8.0);
    let config = WeatherConfig::default();
    let storm = WeatherState::STORM.rates(&config);
    let plain = render(&device, None, eye);
    for resolution in [PrecipResolution::Half, PrecipResolution::Quarter] {
        let rain = render(
            &device,
            Some(frame_at(
                &PrecipIntegrator::default(),
                &storm,
                eye,
                [4.0, 0.0, 1.0],
                resolution,
            )),
            eye,
        );
        let share = coverage(&plain, &rain);
        assert!(
            (0.001..0.7).contains(&share),
            "{} rain covers {share}",
            resolution.name()
        );
    }
}

/// Blizzard snow brightens the frame.
#[test]
fn blizzard_snow_brightens_the_frame() {
    let device = device();
    let eye = Vec3::new(0.0, 1.7, 8.0);
    let config = WeatherConfig::default();
    let blizzard = WeatherState::BLIZZARD.rates(&config);
    let plain = render(&device, None, eye);
    let snow = render(
        &device,
        Some(frame(
            &PrecipIntegrator::default(),
            &blizzard,
            eye,
            [4.0, 0.0, 1.0],
        )),
        eye,
    );
    assert!(
        mean(&snow) > mean(&plain) + 0.1,
        "{} vs {}",
        mean(&snow),
        mean(&plain)
    );
    assert!(coverage(&plain, &snow) > 0.003, "{}", coverage(&plain, &snow));
}

/// Two runs at the same clock draw the same bytes.
#[test]
fn the_same_clock_draws_the_same_bytes() {
    let device = device();
    let eye = Vec3::new(2.0, 1.7, 8.0);
    let config = WeatherConfig::default();
    let storm = WeatherState::STORM.rates(&config);
    let budget = regia_vfx::PrecipBudget::for_quality(QualityTier::Low);
    let mut integrator = PrecipIntegrator::default();
    for _ in 0..30 {
        integrator.step(&storm, &config, [4.0, 1.0], &budget, DT);
    }
    let a = render(
        &device,
        Some(frame(&integrator, &storm, eye, [4.0, 0.0, 1.0])),
        eye,
    );
    let b = render(
        &device,
        Some(frame(&integrator, &storm, eye, [4.0, 0.0, 1.0])),
        eye,
    );
    assert_eq!(a, b);
}

/// A teleport's first frame has the coverage of a steady frame, within 10 %.
#[test]
fn a_teleport_shows_a_full_field_on_its_first_frame() {
    let device = device();
    let config = WeatherConfig::default();
    let storm = WeatherState::STORM.rates(&config);
    let integrator = PrecipIntegrator::default();
    let eye = Vec3::new(0.0, 1.7, 8.0);
    let plain = render(&device, None, eye);
    let steady = coverage(
        &plain,
        &render(&device, Some(frame(&integrator, &storm, eye, [0.0; 3])), eye),
    );
    // The harness ground is the same under the camera, so the backdrop does not change.
    let far = Vec3::new(36.0 * 3.0 + 7.0, 1.7, 36.0 * 2.0 + 8.0);
    let jumped = frame(&integrator, &storm, far, [0.0; 3]);
    let jumped = coverage(&plain, &render(&device, Some(jumped), eye));
    assert!((steady - jumped).abs() <= 0.1 * steady, "{steady} vs {jumped}");
}

/// Rain to snow over ten seconds: no frame's picture jumps.
#[test]
fn a_rain_to_snow_transition_has_no_jump() {
    let device = device();
    let config = WeatherConfig::default();
    let budget = regia_vfx::PrecipBudget::for_quality(QualityTier::Low);
    let eye = Vec3::new(0.0, 1.7, 8.0);
    let plain = render(&device, None, eye);
    let mut weather = Weather::new(WeatherState::STORM);
    weather.transition_to(
        WeatherState {
            precipitation: 1.0,
            ..WeatherState::BLIZZARD
        },
        10.0,
    );
    let mut integrator = PrecipIntegrator::default();
    let mut last = None;
    for step in 0..21 {
        let rates = weather.rates(&config);
        let drawn = render(&device, Some(frame(&integrator, &rates, eye, [0.0; 3])), eye);
        let share = coverage(&plain, &drawn);
        if let Some(last) = last {
            let moved: f32 = share - last;
            assert!(moved.abs() < 0.2, "step {step}: {last} to {share}");
        }
        last = Some(share);
        for _ in 0..30 {
            weather.step(DT);
            integrator.step(&rates, &config, [0.0, 0.0], &budget, DT);
        }
    }
}

/// Writes the storm and the blizzard to `REGIA_PRECIP_SHEET_DIR` for a look by eye. Not a test of
/// the picture: `cargo test -p regia_vfx --test precipitation sheet -- --ignored`.
#[test]
#[ignore = "writes images for a look by eye"]
fn sheet() {
    let Some(dir) = std::env::var_os("REGIA_PRECIP_SHEET_DIR") else {
        return;
    };
    let device = device();
    let eye = Vec3::new(0.0, 1.7, 8.0);
    let config = WeatherConfig::default();
    let budget = regia_vfx::PrecipBudget::for_quality(QualityTier::High);
    for (name, state) in [
        ("storm", WeatherState::STORM),
        ("rain", WeatherState::RAIN),
        ("blizzard", WeatherState::BLIZZARD),
    ] {
        let rates = state.rates(&config);
        let mut integrator = PrecipIntegrator::default();
        integrator.step(&rates, &config, [4.0, 1.0], &budget, DT);
        let frame = integrator.frame(
            [f64::from(eye.x), f64::from(eye.y), f64::from(eye.z)],
            [0.0; 3],
            [0.0, -1.0],
            0.0,
            &rates,
            &config,
            &settings(),
            [4.0, 0.0, 1.0],
            QualityTier::High,
        );
        write_sheet(&dir, name, &render(&device, Some(frame), eye));
    }
    write_sheet(&dir, "plain", &render(&device, None, eye));
}

fn write_sheet(dir: &std::ffi::OsStr, name: &str, pixels: &[u8]) {
    let path = std::path::Path::new(dir).join(format!("{name}.png"));
    let file = std::fs::File::create(&path).expect("file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .expect("header")
        .write_image_data(pixels)
        .expect("pixels");
}
