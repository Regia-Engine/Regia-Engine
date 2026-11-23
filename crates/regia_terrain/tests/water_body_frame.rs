// SPDX-License-Identifier: MPL-2.0
//! Water bodies in the runtime frame (ADR-1399), through the real `FrameRenderer` and its
//! translucent-layer seam on a Desktop-High-shaped frame (single sample, `Rgba16Float`, post on, EV
//! 14): a sloped stream over a pebble bed with three boulders.
//!
//! No golden image: every check compares frames of the same run (with and without water, two
//! ticks, two tiers) or two renders of the same tick (byte-identical). `REGIA_WATER_FRAME_DUMP=<dir>`
//! writes the frames as PNGs (evidence; not compared).

mod water_scene;

use regia_render::{FrameRenderer, RenderDevice};
use regia_terrain::water_body::WaterFrameStats;
use regia_terrain::{IceShelf, WaterClock, WaterRenderer, WaterTier};
use water_scene::{
    Camera, FAR, NEAR, STUDIO_SKY, StreamScene, TOP, frame, ground, post, project, stream_scene, surface,
};

const W: u32 = 256;
const H: u32 = 144;
/// The sun's travel direction: about 40° up, from behind the camera's right.
const SUN: [f32; 3] = [0.35, -0.65, -0.55];

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

#[derive(Clone, Copy)]
struct Shot {
    /// `None`: no water layer at all.
    tier: Option<WaterTier>,
    tick: u64,
    ice: bool,
    /// A mirror-calm surface without obstacles (no ripples, no refraction offset).
    calm: bool,
    camera: Camera,
}

impl Shot {
    fn near(tier: Option<WaterTier>, tick: u64) -> Self {
        Self {
            tier,
            tick,
            ice: false,
            calm: false,
            camera: NEAR,
        }
    }
}

fn render(device: &RenderDevice, scene: &StreamScene, shot: Shot, label: &str) -> (Vec<u8>, WaterFrameStats) {
    let mut renderer = FrameRenderer::new();
    renderer.set_studio_sky(device, Some(STUDIO_SKY));
    let water = WaterRenderer::new(device);
    if let Some(tier) = shot.tier {
        water.set_tier(tier);
        water.set_clock(WaterClock {
            tick: shot.tick,
            ..WaterClock::default()
        });
        let mut stream = scene.stream.clone();
        if shot.ice {
            stream.ice = Some(IceShelf::default());
        }
        if shot.calm {
            stream.look.normal_strength = 0.0;
            stream.obstacles.clear();
        }
        water.set_bodies(&[&stream]);
        water.set_ambient_lux(18_000.0);
        renderer.set_translucent_layers(vec![water.layer()]);
    }
    let post = post();
    let pixels = renderer
        .render_offscreen(device, &frame(scene, shot.camera, W, H, &post, SUN))
        .expect("frame")
        .pixels;
    dump(label, &pixels);
    (pixels, water.stats())
}

fn dump(label: &str, pixels: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_WATER_FRAME_DUMP") else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = std::path::Path::new(&dir).join(format!("{label}.png"));
    let Ok(file) = std::fs::File::create(&path) else {
        return;
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn luma_at(pixels: &[u8], x: u32, y: u32) -> f32 {
    let i = ((y.min(H - 1) * W + x.min(W - 1)) * 4) as usize;
    (0.2126 * f32::from(pixels[i]) + 0.7152 * f32::from(pixels[i + 1]) + 0.0722 * f32::from(pixels[i + 2]))
        / 255.0
}

/// Mean luma at the pixels of `points` (those on screen).
fn mean_luma(pixels: &[u8], camera: Camera, points: &[[f32; 3]]) -> f32 {
    let hits: Vec<f32> = points
        .iter()
        .filter_map(|p| project(camera, W, H, *p))
        .map(|(x, y)| luma_at(pixels, x, y))
        .collect();
    assert!(!hits.is_empty(), "no point on screen");
    hits.iter().sum::<f32>() / hits.len() as f32
}

/// Mean absolute difference (0..1) over the pixels of `points`.
fn mean_diff(a: &[u8], b: &[u8], camera: Camera, points: &[[f32; 3]]) -> f32 {
    let hits: Vec<f32> = points
        .iter()
        .filter_map(|p| project(camera, W, H, *p))
        .map(|(x, y)| (luma_at(a, x, y) - luma_at(b, x, y)).abs())
        .collect();
    assert!(!hits.is_empty(), "no point on screen");
    hits.iter().sum::<f32>() / hits.len() as f32
}

/// Points on the water surface across the stream between `x0` and `x1`.
fn water_points(x0: f32, x1: f32) -> Vec<[f32; 3]> {
    let mut points = Vec::new();
    let mut x = x0;
    while x <= x1 {
        for z in [-1.0f32, -0.5, 0.0, 0.5, 1.0] {
            points.push([x, surface(x), z]);
        }
        x += 0.5;
    }
    points
}

/// Ground points (on the far bank) at lateral distances `z0..z1` (negative side).
fn bank_points(z0: f32, z1: f32) -> Vec<[f32; 3]> {
    let mut points = Vec::new();
    let mut x = 3.0f32;
    while x <= 9.0 {
        let mut z = z0;
        while z <= z1 + 1.0e-4 {
            points.push([x, ground(x, -z), -z]);
            z += 0.02;
        }
        x += 0.25;
    }
    points
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The water is drawn, and two fresh renders of the same tick are byte-identical.
#[test]
fn water_draws_and_rerenders_are_hash_stable() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let (bare, none) = render(&device, &scene, Shot::near(None, 0), "near_bare");
    assert_eq!(none.frames_drawn, 0);
    let (a, stats) = render(
        &device,
        &scene,
        Shot::near(Some(WaterTier::High), 120),
        "near_high_t120",
    );
    let (b, _) = render(
        &device,
        &scene,
        Shot::near(Some(WaterTier::High), 120),
        "near_high_t120_again",
    );
    assert_eq!(fnv1a(&a), fnv1a(&b), "two renders of tick 120 differ");
    assert!(stats.frames_drawn >= 1, "{stats:?}");
    assert!(
        stats.composite,
        "the High tier composites on an HDR frame: {stats:?}"
    );
    assert_eq!(stats.obstacles, 3);
    assert_eq!(stats.tick, 120);
    let diff = mean_diff(&bare, &a, NEAR, &water_points(1.0, 9.0));
    assert!(
        diff > 0.02,
        "the stream looks the same with and without water ({diff})"
    );
    // The dry bank far from the stream is untouched.
    let dry = mean_diff(&bare, &a, NEAR, &bank_points(3.4, 3.6));
    assert!(dry < 0.004, "water drew on the dry bank ({dry})");
}

/// The flow moves the surface from tick to tick; the banks do not change.
#[test]
fn water_moves_with_the_fixed_step_tick() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let (a, _) = render(
        &device,
        &scene,
        Shot::near(Some(WaterTier::High), 120),
        "near_moving_t120",
    );
    let (b, _) = render(
        &device,
        &scene,
        Shot::near(Some(WaterTier::High), 150),
        "near_moving_t150",
    );
    let moving = mean_diff(&a, &b, NEAR, &water_points(1.0, 9.0));
    assert!(
        moving > 0.005,
        "half a second later the water is unchanged ({moving})"
    );
    let dry = mean_diff(&a, &b, NEAR, &bank_points(3.4, 3.6));
    assert!(dry < 0.002, "the dry bank changed with the tick ({dry})");
}

/// Through 35 cm of clear, calm water the pebbles still read: light pebbles stay well brighter than
/// dark ones (Beer-Lambert absorption and Fresnel, not an opaque sheet).
#[test]
fn pebble_bed_shows_through_shallow_water() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let top = |tier: Option<WaterTier>, calm: bool| Shot {
        tier,
        tick: 60,
        ice: false,
        calm,
        camera: TOP,
    };
    let (bare, _) = render(&device, &scene, top(None, false), "top_pebbles_bare");
    let (wet, _) = render(
        &device,
        &scene,
        top(Some(WaterTier::High), true),
        "top_pebbles_calm",
    );
    // Evidence only: the same view with ripples and standing waves.
    let _ = render(
        &device,
        &scene,
        top(Some(WaterTier::High), false),
        "top_pebbles_flowing",
    );
    let pick = |light: bool| -> Vec<[f32; 3]> {
        scene
            .pebbles
            .iter()
            .filter(|(p, l)| *l == light && p[2].abs() < 1.2 && project(TOP, W, H, *p).is_some())
            .map(|(p, _)| *p)
            .collect()
    };
    let (light, dark) = (pick(true), pick(false));
    let contrast_bare = mean_luma(&bare, TOP, &light) - mean_luma(&bare, TOP, &dark);
    let contrast_wet = mean_luma(&wet, TOP, &light) - mean_luma(&wet, TOP, &dark);
    assert!(
        contrast_bare > 0.1,
        "the test bed itself has no contrast ({contrast_bare})"
    );
    assert!(
        contrast_wet > 0.35 * contrast_bare,
        "the pebbles vanish under the water ({contrast_bare} → {contrast_wet})"
    );
}

/// The Low tier (Deck, mobile) blends without a scene copy; it still draws the water and is just as
/// deterministic.
#[test]
fn low_tier_blends_and_is_deterministic() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let (bare, _) = render(&device, &scene, Shot::near(None, 0), "near_low_bare");
    let (a, stats) = render(
        &device,
        &scene,
        Shot::near(Some(WaterTier::Low), 90),
        "near_low_t90",
    );
    let (b, _) = render(
        &device,
        &scene,
        Shot::near(Some(WaterTier::Low), 90),
        "near_low_t90_again",
    );
    assert!(!stats.composite, "{stats:?}");
    assert_eq!(stats.obstacles, 0);
    assert_eq!(fnv1a(&a), fnv1a(&b));
    let diff = mean_diff(&bare, &a, NEAR, &water_points(1.0, 9.0));
    assert!(diff > 0.02, "the Low tier drew no water ({diff})");
}

/// Wet banks: the ground just above the waterline is darker with the water than without; an ice
/// shelf brightens the stream's edges.
#[test]
fn wet_banks_darken_and_ice_brightens_the_edges() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let (bare, _) = render(&device, &scene, Shot::near(None, 0), "near_banks_bare");
    let (wet, _) = render(
        &device,
        &scene,
        Shot::near(Some(WaterTier::High), 60),
        "near_banks_water",
    );
    let band = bank_points(1.83, 1.9);
    let (before, after) = (mean_luma(&bare, NEAR, &band), mean_luma(&wet, NEAR, &band));
    assert!(
        after < 0.97 * before,
        "the bank above the waterline is not wet ({before} → {after})"
    );
    let iced = Shot {
        ice: true,
        ..Shot::near(Some(WaterTier::High), 60)
    };
    let (ice, _) = render(&device, &scene, iced, "near_ice");
    // The near bank's edge, where the shelf is a broad strip in this view.
    let edge: Vec<[f32; 3]> = (0..40)
        .flat_map(|i| {
            let x = 2.0 + 0.15 * i as f32;
            [1.55f32, 1.6, 1.65, 1.7, 1.75].map(|z| [x, surface(x), z])
        })
        .collect();
    let (open, frozen) = (mean_luma(&wet, NEAR, &edge), mean_luma(&ice, NEAR, &edge));
    assert!(
        frozen > open * 1.05 + 0.01,
        "no ice at the edge ({open} → {frozen})"
    );
}

/// From about 200 m the stream is still drawn, still moves, and re-renders identically.
#[test]
fn stream_reads_from_200_m() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let far = |tier: Option<WaterTier>, tick: u64| Shot {
        tier,
        tick,
        ice: false,
        calm: false,
        camera: FAR,
    };
    let (bare, _) = render(&device, &scene, far(None, 0), "far_bare");
    let (a, _) = render(&device, &scene, far(Some(WaterTier::High), 120), "far_t120");
    let (b, _) = render(&device, &scene, far(Some(WaterTier::High), 120), "far_t120_again");
    let (c, _) = render(&device, &scene, far(Some(WaterTier::High), 180), "far_t180");
    assert_eq!(fnv1a(&a), fnv1a(&b));
    let centerline: Vec<[f32; 3]> = (0..60)
        .map(|i| {
            let x = -4.0 + 0.6 * i as f32;
            [x, surface(x), 0.0]
        })
        .collect();
    let drawn = mean_diff(&bare, &a, FAR, &centerline);
    assert!(drawn > 0.02, "no water at 200 m ({drawn})");
    let moving = mean_diff(&a, &c, FAR, &centerline);
    assert!(moving > 0.001, "the stream is frozen at 200 m ({moving})");
}
