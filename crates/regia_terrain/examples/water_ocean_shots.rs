// SPDX-License-Identifier: MPL-2.0
//! FFT ocean evidence (ADR-1969).
//!
//! Renders a 4 km sea beside the stream scene through `FrameRenderer` on the default GPU and writes
//! PNGs of the same views with the swell alone (the stand-in, what Low draws) and with the ocean,
//! on High and Ultra, at the 512 and 1024 grids (ADR-2126), plus the `normal` and `masks` debug views
//! (foam in red). Prints each frame's
//! hash. Then it times the CPU mirror, the surface query and, with `--frames`, the frame, per tier.
//! Evidence for a look by eye and for the cost table: nothing here is compared.
//!
//! ```text
//! cargo run --release -p regia_terrain --example water_ocean_shots -- <out dir> [--size 1280x720] [--frames 16]
//! ```

#[path = "../tests/water_scene/mod.rs"]
mod water_scene;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use regia_render::{FrameRenderer, RenderDevice};
use regia_terrain::water_body::WaterDebug;
use regia_terrain::water_body::ocean::{OCEAN_QUERY_SIZE, OceanFrame, loop_fraction, tables_for};
use regia_terrain::{FftOcean, WaterBody, WaterClock, WaterLook, WaterRenderer, WaterTier};
use water_scene::{Camera, STUDIO_SKY, StreamScene, frame, post, stream_scene};

const SUN: [f32; 3] = [0.35, -0.65, -0.55];
const LEVEL: f32 = -0.1;

const BOAT: Camera = Camera {
    eye: [-20.0, 4.0, 0.0],
    target: [-120.0, LEVEL, -10.0],
    fov_y: 0.9,
};
const CLIFF: Camera = Camera {
    eye: [-10.0, 40.0, 30.0],
    target: [-200.0, LEVEL, -60.0],
    fov_y: 0.9,
};

fn ocean() -> FftOcean {
    FftOcean {
        wind_speed: 14.0,
        wind_direction: [-1.0, -0.25],
        seed: 11,
        ..FftOcean::default()
    }
}

fn sea() -> WaterBody {
    sea_on_grid(0)
}

fn sea_on_grid(grid: u32) -> WaterBody {
    let mut sea = WaterBody::plane([0.0, LEVEL, 0.0], [2000.0, 2000.0], WaterLook::ocean());
    sea.ocean = Some(FftOcean { grid, ..ocean() });
    sea
}

struct Args {
    out: PathBuf,
    size: (u32, u32),
    frames: u32,
}

fn parse_args() -> Result<Args, Box<dyn Error>> {
    let mut it = std::env::args().skip(1);
    let mut out = None;
    let mut size = (1280, 720);
    let mut frames = 0;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--size" => {
                let v = it.next().ok_or("--size needs WxH")?;
                let (w, h) = v.split_once('x').ok_or("--size needs WxH")?;
                size = (w.parse()?, h.parse()?);
            }
            "--frames" => frames = it.next().ok_or("--frames needs a number")?.parse()?,
            _ => out = Some(PathBuf::from(arg)),
        }
    }
    Ok(Args {
        out: out.ok_or("usage: water_ocean_shots <out dir> [--size WxH] [--frames N]")?,
        size,
        frames,
    })
}

fn write_png(path: &Path, size: (u32, u32), pixels: &[u8]) -> Result<(), Box<dyn Error>> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), size.0, size.1);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(pixels)?;
    Ok(())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

struct Shot {
    camera: Camera,
    tier: WaterTier,
    ocean: bool,
    debug: WaterDebug,
    tick: u64,
    grid: u32,
}

fn shot(
    device: &RenderDevice,
    scene: &StreamScene,
    shot: &Shot,
    size: (u32, u32),
) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut renderer = FrameRenderer::new();
    renderer.set_studio_sky(device, Some(STUDIO_SKY));
    let water = WaterRenderer::new(device);
    let sea = sea_on_grid(shot.grid);
    water.set_tier(shot.tier);
    water.set_clock(WaterClock {
        tick: shot.tick,
        ..WaterClock::default()
    });
    water.set_bodies(&[&sea]);
    water.set_ocean_draw(shot.ocean);
    water.set_debug(shot.debug);
    water.set_ambient_lux(18_000.0);
    renderer.set_translucent_layers(vec![water.layer()]);
    let post = post();
    let out = renderer.render_offscreen(device, &frame(scene, shot.camera, size.0, size.1, &post, SUN))?;
    Ok(out.pixels)
}

/// The mean wall time of `f` over `n` runs (ms).
fn timed(n: u32, mut f: impl FnMut(u32)) -> f64 {
    let start = Instant::now();
    for i in 0..n {
        f(i);
    }
    start.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = parse_args()?;
    std::fs::create_dir_all(&args.out)?;
    let device = RenderDevice::headless()?;
    let scene = stream_scene();
    println!(
        "ocean: Hs {:.2} m, peak {:.0} m",
        ocean().expected_height(),
        ocean().peak_wavelength()
    );
    for (view, camera) in [("boat", BOAT), ("cliff", CLIFF)] {
        let modes = [
            ("swell_high", WaterTier::High, false, WaterDebug::Off, 0),
            ("ocean_high", WaterTier::High, true, WaterDebug::Off, 0),
            ("ocean_ultra", WaterTier::Ultra, true, WaterDebug::Off, 0),
            ("ocean_grid512", WaterTier::High, true, WaterDebug::Off, 512),
            ("ocean_grid1024", WaterTier::High, true, WaterDebug::Off, 1024),
            ("ocean_normal", WaterTier::High, true, WaterDebug::Normal, 0),
            ("ocean_masks", WaterTier::High, true, WaterDebug::Masks, 0),
            (
                "ocean_masks_grid1024",
                WaterTier::High,
                true,
                WaterDebug::Masks,
                1024,
            ),
        ];
        for (name, tier, ocean, debug, grid) in modes {
            let pixels = shot(
                &device,
                &scene,
                &Shot {
                    camera,
                    tier,
                    ocean,
                    debug,
                    tick: 3600,
                    grid,
                },
                args.size,
            )?;
            write_png(&args.out.join(format!("{view}_{name}.png")), args.size, &pixels)?;
            println!("{view}_{name}: {:016x}", fnv1a(&pixels));
        }
    }

    // The CPU side.
    let o = ocean();
    let start = Instant::now();
    let tables128 = Arc::new(regia_terrain::water_body::OceanTables::new(&o, OCEAN_QUERY_SIZE));
    let tables_ms = start.elapsed().as_secs_f64() * 1000.0;
    let modes: usize = tables128.bands.iter().map(|b| b.modes.len()).sum();
    println!("tables 128: {tables_ms:.1} ms, {modes} modes");
    let mirror = timed(32, |i| {
        let f = OceanFrame::at(&tables128, loop_fraction(f64::from(i) / 60.0));
        std::hint::black_box(f.rise);
    });
    println!("mirror 128, fresh buffers: {mirror:.2} ms per frame");
    let maps = OceanFrame::at(&tables_for(&o, OCEAN_QUERY_SIZE), loop_fraction(1.0));
    let sample_us = timed(100_000, |i| {
        std::hint::black_box(maps.sample(i as f32 * 0.37, i as f32 * -0.21));
    }) * 1000.0;
    println!("sample: {sample_us:.3} us");
    println!(
        "foamed (J under 1 - onset): {:.2} % of the longest tile",
        100.0 * maps.compressed_share(1.0 - o.foam_onset, 256)
    );
    let sea = sea();
    let clock = WaterClock {
        tick: 60,
        ..WaterClock::default()
    };
    let _ = sea.surface(0.0, 0.0, &clock);
    let surface_us = timed(10_000, |i| {
        std::hint::black_box(sea.surface(i as f32 * 0.09, i as f32 * -0.05, &clock));
    }) * 1000.0;
    println!("surface query, frame cached: {surface_us:.3} us");
    let ticks = timed(32, |i| {
        let c = WaterClock {
            tick: 1000 + u64::from(i),
            ..WaterClock::default()
        };
        for j in 0..800 {
            std::hint::black_box(sea.surface(j as f32 * 0.5, 3.0, &c));
        }
    });
    println!("one tick of 800 queries, mirror included: {ticks:.2} ms");

    // The frame, per tier.
    if args.frames > 0 {
        println!(
            "frame ({}x{}, {} frames), ms wall:",
            args.size.0, args.size.1, args.frames
        );
        for tier in [WaterTier::Low, WaterTier::High, WaterTier::Ultra] {
            for draw in [false, true] {
                let mut renderer = FrameRenderer::new();
                renderer.set_studio_sky(&device, Some(STUDIO_SKY));
                let water = WaterRenderer::new(&device);
                water.set_tier(tier);
                water.set_bodies(&[&sea]);
                water.set_ocean_draw(draw);
                water.set_ambient_lux(18_000.0);
                renderer.set_translucent_layers(vec![water.layer()]);
                renderer
                    .register_pass(water.ocean_pass())
                    .map_err(|e| e.to_string())?;
                let post = post();
                let mut wall = 0.0;
                for i in 0..(args.frames + 4) {
                    water.set_clock(WaterClock {
                        tick: 600 + u64::from(i),
                        ..WaterClock::default()
                    });
                    let start = Instant::now();
                    renderer.render_offscreen(
                        &device,
                        &frame(&scene, BOAT, args.size.0, args.size.1, &post, SUN),
                    )?;
                    if i >= 4 {
                        wall += start.elapsed().as_secs_f64() * 1000.0;
                    }
                }
                println!(
                    "  {:6} ocean_draw {draw:5}: {:.1}",
                    tier.name(),
                    wall / f64::from(args.frames)
                );
            }
        }
    }
    Ok(())
}
