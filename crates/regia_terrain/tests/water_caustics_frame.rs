// SPDX-License-Identifier: MPL-2.0
//! Water caustics and sun shafts (ADR-2432) on a real GPU (lavapipe in CI).
//!
//! Off by default: the default frames equal the frames with the setting present but disabled, and
//! no pass is recorded. On, the frame differs only inside the water's footprint (the pixels the
//! water layer changes at all), and the shafts differ only with the eye under a surface. The tile
//! the GPU writes equals the CPU mirror. No golden image: every check compares frames of the same
//! run. `REGIA_WATER_FRAME_DUMP=<dir>` writes the before/after plates as PNGs for a look by eye
//! (evidence; not compared).

mod water_scene;

use regia_render::{FrameRenderer, RenderDevice};
use regia_terrain::water_body::caustics::{self, CAUSTIC_TILE_SIZE};
use regia_terrain::water_body::normal_map::ripple_levels;
use regia_terrain::{
    CausticStats, Swell, WaterBody, WaterCaustics, WaterClock, WaterLook, WaterRenderer, WaterTier,
};
use water_scene::{Camera, STUDIO_SKY, StreamScene, frame, post, stream_scene};

const W: u32 = 256;
const H: u32 = 144;
/// The direction the light travels, as the frame's directional light takes it.
const SUN: [f32; 3] = [0.35, -0.65, -0.55];
const LEVEL: f32 = -0.1;
const TICK: u64 = 600;

static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn sea(level: f32, caustics: WaterCaustics) -> WaterBody {
    let mut look = WaterLook::ocean();
    look.caustics = caustics;
    let mut sea = WaterBody::plane([0.0, level, 0.0], [60.0, 60.0], look);
    sea.swell = Some(Swell::open_sea([1.0, 0.35], 0.35, 6.0, 20));
    sea
}

/// Clear water: shafts need light that survives a few metres.
fn clear_sea(level: f32, caustics: WaterCaustics) -> WaterBody {
    let mut body = sea(level, caustics);
    body.look.absorption = [0.12, 0.04, 0.03];
    body.look.scattering = 0.2;
    // Shafts and focus need ripples a metre or so long; the sea's 18 m tile focuses at ~5 m.
    body.look.ripple_scale = 1.5;
    body
}

/// A shallow sea with stream-sized ripples, for the plates of the bed.
fn ripple_sea(level: f32, caustics: WaterCaustics) -> WaterBody {
    let mut body = sea(level, caustics);
    body.look.ripple_scale = 1.5;
    body
}

fn on() -> WaterCaustics {
    WaterCaustics {
        enabled: true,
        strength: 2.0,
        ..WaterCaustics::default()
    }
}

/// Over the sea from 3 m up, looking along it.
const ABOVE: Camera = Camera {
    eye: [-6.0, 3.0, 4.0],
    target: [8.0, LEVEL, -1.0],
    fov_y: 0.9,
};

/// The sea of the underwater frames: high over the scene, so the eye is in the water column.
const HIGH_LEVEL: f32 = 3.0;

/// Looking toward the sun from under the surface (forward scattering shows the shafts there).
const UNDER: Camera = Camera {
    eye: [-6.0, 1.0, 4.0],
    target: [-8.1, 5.0, 7.3],
    fov_y: 1.1,
};

struct Shot {
    pixels: Vec<u8>,
    caustics: CausticStats,
    tile: Option<Vec<[f32; 4]>>,
}

/// `body: None` renders the scene with the water layer installed but no body drawn.
fn render(
    device: &RenderDevice,
    scene: &StreamScene,
    camera: Camera,
    body: Option<&WaterBody>,
    tier: WaterTier,
) -> Shot {
    render_at(device, scene, camera, body, tier, (W, H))
}

fn render_at(
    device: &RenderDevice,
    scene: &StreamScene,
    camera: Camera,
    body: Option<&WaterBody>,
    tier: WaterTier,
    size: (u32, u32),
) -> Shot {
    let mut renderer = FrameRenderer::new();
    renderer.set_studio_sky(device, Some(STUDIO_SKY));
    let water = WaterRenderer::new(device);
    water.set_tier(tier);
    water.set_underwater(true);
    water.set_clock(WaterClock {
        tick: TICK,
        ..WaterClock::default()
    });
    if let Some(body) = body {
        water.set_bodies(&[body]);
    }
    water.set_ambient_lux(18_000.0);
    renderer.set_translucent_layers(vec![water.layer()]);
    let post = post();
    let pixels = renderer
        .render_offscreen(device, &frame(scene, camera, size.0, size.1, &post, SUN))
        .expect("frame")
        .pixels;
    Shot {
        pixels,
        caustics: water.caustic_stats(),
        tile: water.read_caustic_tile(),
    }
}

/// Pixels where any channel differs by more than `tolerance`.
fn differs(a: &[u8], b: &[u8], tolerance: u8) -> Vec<bool> {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .map(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > tolerance))
        .collect()
}

fn count(mask: &[bool]) -> usize {
    mask.iter().filter(|m| **m).count()
}

fn dump(label: &str, pixels: &[u8]) {
    dump_sized(label, pixels, (W, H));
}

fn dump_sized(label: &str, pixels: &[u8], size: (u32, u32)) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_WATER_FRAME_DUMP") else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = std::path::Path::new(&dir).join(format!("caustics_{label}.png"));
    let Ok(file) = std::fs::File::create(&path) else {
        return;
    };
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), size.0, size.1);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    if let Ok(mut writer) = encoder.write_header() {
        let _ = writer.write_image_data(pixels);
    }
}

/// With the setting off (the default, or present but disabled) the frame is the same bytes and no
/// pass is recorded or built.
#[test]
fn caustics_off_changes_no_pixel_and_records_nothing() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let plain = render(
        &device,
        &scene,
        ABOVE,
        Some(&sea(LEVEL, WaterCaustics::default())),
        WaterTier::High,
    );
    let disabled = render(
        &device,
        &scene,
        ABOVE,
        Some(&sea(
            LEVEL,
            WaterCaustics {
                enabled: false,
                strength: 3.0,
                chromatic: 1.0,
                ..WaterCaustics::default()
            },
        )),
        WaterTier::High,
    );
    assert_eq!(plain.pixels, disabled.pixels, "a disabled setting changed pixels");
    assert_eq!(plain.caustics, CausticStats::default(), "{:?}", plain.caustics);
    assert_eq!(
        disabled.caustics,
        CausticStats::default(),
        "{:?}",
        disabled.caustics
    );
    assert!(plain.tile.is_none());
}

/// On, the frame differs from off only inside the water's footprint, and does differ there.
#[test]
fn caustics_change_pixels_only_inside_the_water() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let none = render(&device, &scene, ABOVE, None, WaterTier::High);
    let off = render(
        &device,
        &scene,
        ABOVE,
        Some(&sea(LEVEL, WaterCaustics::default())),
        WaterTier::High,
    );
    let lit = render(&device, &scene, ABOVE, Some(&sea(LEVEL, on())), WaterTier::High);
    let footprint = differs(&none.pixels, &off.pixels, 0);
    let changed = differs(&off.pixels, &lit.pixels, 2);
    let outside = changed
        .iter()
        .zip(&footprint)
        .filter(|(c, f)| **c && !**f)
        .count();
    assert_eq!(
        outside, 0,
        "{outside} pixels changed outside the water's footprint"
    );
    assert!(
        count(&changed) > count(&footprint) / 50,
        "caustics changed {} of {} water pixels",
        count(&changed),
        count(&footprint)
    );
    assert_eq!(lit.caustics.bodies, 1, "{:?}", lit.caustics);
    assert_eq!(lit.caustics.tile_passes, 1);
    assert_eq!(
        lit.caustics.shaft_passes, 0,
        "no shafts with the eye above the water"
    );
    dump("caustics_off", &off.pixels);
    dump("caustics_on", &lit.pixels);
}

/// The compute tile equals its CPU mirror (the same ripple map, scroll and sun), and its density
/// at a metre keeps the energy: a mean near 1 and a focus well above it.
#[test]
fn the_gpu_tile_matches_the_cpu_mirror() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let lit = render(&device, &scene, ABOVE, Some(&sea(LEVEL, on())), WaterTier::High);
    let tile = lit.tile.expect("a tile was written");
    assert_eq!(tile.len(), (CAUSTIC_TILE_SIZE * CAUSTIC_TILE_SIZE) as usize);
    let norm = (SUN[0] * SUN[0] + SUN[1] * SUN[1] + SUN[2] * SUN[2]).sqrt();
    let to_sun = [-SUN[0] / norm, -SUN[1] / norm, -SUN[2] / norm];
    let clock = WaterClock {
        tick: TICK,
        ..WaterClock::default()
    };
    let cpu = caustics::cpu_tile(&ripple_levels()[0], &caustics::tile_params(&clock, 1.0, to_sun));
    // The trace is the Jacobian's scale (a few per tile); the offset channels are ~0.1.
    let mut worst = [0.0f32; 4];
    for (g, c) in tile.iter().zip(&cpu) {
        for k in 0..4 {
            worst[k] = worst[k].max((g[k] - c[k]).abs());
        }
    }
    assert!(
        worst[0] < 0.5 && worst[1] < 2.0,
        "tile differs from the CPU mirror: {worst:?}"
    );
    assert!(worst[2] < 0.02 && worst[3] < 0.02, "offsets differ: {worst:?}");
    let (mean, max) = caustics::tile_density_summary(&tile, 0.27);
    assert!((0.9..1.1).contains(&mean) && max > 1.5, "mean {mean} max {max}");
}

/// Shafts are drawn only with the eye under a surface, and only when asked for: steps 0 matches
/// the same scene without shafts, steps 12 differs, and the tier Low compiles everything out.
#[test]
fn shafts_appear_only_under_the_surface() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let no_shafts = WaterCaustics {
        shaft_steps: 0,
        ..on()
    };
    let shafts = WaterCaustics {
        shaft_steps: 12,
        ..on()
    };
    // Eye above water: shaft_steps changes nothing and records no pass.
    let a = render(
        &device,
        &scene,
        ABOVE,
        Some(&sea(LEVEL, no_shafts)),
        WaterTier::High,
    );
    let b = render(&device, &scene, ABOVE, Some(&sea(LEVEL, shafts)), WaterTier::High);
    assert_eq!(a.pixels, b.pixels, "shafts drawn with the eye above the water");
    assert_eq!(b.caustics.shaft_passes, 0);
    // Eye under water: the shafts add light.
    let under_off = render(
        &device,
        &scene,
        UNDER,
        Some(&clear_sea(HIGH_LEVEL, no_shafts)),
        WaterTier::High,
    );
    let under_on = render(
        &device,
        &scene,
        UNDER,
        Some(&clear_sea(HIGH_LEVEL, shafts)),
        WaterTier::High,
    );
    dump("shafts_off", &under_off.pixels);
    dump("shafts_on", &under_on.pixels);
    assert_eq!(under_on.caustics.shaft_passes, 1, "{:?}", under_on.caustics);
    assert!(under_on.caustics.shafts_drawn);
    assert_eq!(under_on.caustics.shaft_steps, 12);
    assert_eq!(
        under_on.caustics.shaft_texels,
        u64::from(W / 2) * u64::from(H / 2)
    );
    assert_eq!(under_off.caustics.shaft_passes, 0);
    let changed = differs(&under_off.pixels, &under_on.pixels, 2);
    let bright = |p: &[u8]| {
        p.as_chunks::<4>()
            .0
            .iter()
            .map(|q| u64::from(q[0]) + u64::from(q[1]) + u64::from(q[2]))
            .sum::<u64>()
    };
    let (b_on, b_off) = (bright(&under_on.pixels), bright(&under_off.pixels));
    assert!(
        count(&changed) > 100,
        "{} pixels changed (brightness {b_on} vs {b_off})",
        count(&changed)
    );
    assert!(b_on > b_off, "shafts only add light ({b_on} vs {b_off})");
    // Low compiles the pass out: no tile, no shaft, pixels of the plain look.
    let low_on = render(
        &device,
        &scene,
        UNDER,
        Some(&clear_sea(HIGH_LEVEL, shafts)),
        WaterTier::Low,
    );
    let low_off = render(
        &device,
        &scene,
        UNDER,
        Some(&clear_sea(HIGH_LEVEL, WaterCaustics::default())),
        WaterTier::Low,
    );
    assert_eq!(low_on.pixels, low_off.pixels, "Low drew caustics");
    assert_eq!(low_on.caustics, CausticStats::default());
}

/// The warm key: warming the water's pipelines builds the same set whatever the caustics setting,
/// and no compute pipeline exists until a frame with the pass on has run.
#[test]
fn warming_builds_no_caustic_pipeline() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let water = WaterRenderer::new(&device);
    water.set_bodies(&[&sea(LEVEL, on())]);
    water
        .warm(&[wgpu::TextureFormat::Rgba16Float, wgpu::TextureFormat::Rgba8Unorm])
        .expect("warm");
    assert_eq!(water.caustic_stats(), CausticStats::default());
    assert!(water.read_caustic_tile().is_none());
}

/// Plates for the owner's look review (ignored: writes files, asserts only that they differ):
/// `REGIA_WATER_FRAME_DUMP=<dir> cargo test -p regia_terrain --test water_caustics_frame -- --ignored caustics_plates`
/// writes `caustics_plate_*.png`: the bed from above and the view toward the sun from under the
/// surface, each off and on, at 960 x 540.
#[test]
#[ignore = "writes plates for a look by eye"]
fn caustics_plates() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let scene = stream_scene();
    let size = (960, 540);
    let down = Camera {
        eye: [-2.5, 1.6, 2.0],
        target: [2.0, -0.4, -0.5],
        fov_y: 0.9,
    };
    let strong = WaterCaustics {
        strength: 1.5,
        ..on()
    };
    let off = render_at(
        &device,
        &scene,
        down,
        Some(&ripple_sea(LEVEL, WaterCaustics::default())),
        WaterTier::High,
        size,
    );
    let lit = render_at(
        &device,
        &scene,
        down,
        Some(&ripple_sea(LEVEL, strong)),
        WaterTier::High,
        size,
    );
    dump_sized("plate_bed_off", &off.pixels, size);
    dump_sized("plate_bed_on", &lit.pixels, size);
    assert_ne!(off.pixels, lit.pixels);
    let clear_off = render_at(
        &device,
        &scene,
        UNDER,
        Some(&clear_sea(
            HIGH_LEVEL,
            WaterCaustics {
                shaft_steps: 0,
                ..strong
            },
        )),
        WaterTier::High,
        size,
    );
    let clear_on = render_at(
        &device,
        &scene,
        UNDER,
        Some(&clear_sea(
            HIGH_LEVEL,
            WaterCaustics {
                shaft_steps: 16,
                ..strong
            },
        )),
        WaterTier::High,
        size,
    );
    dump_sized("plate_shafts_off", &clear_off.pixels, size);
    dump_sized("plate_shafts_on", &clear_on.pixels, size);
    assert_ne!(clear_off.pixels, clear_on.pixels);
}
