// SPDX-License-Identifier: MPL-2.0
//! What the FFT ocean costs at each grid on a real card (ADR-2126). Hand-run evidence for the cost
//! table: nothing here is compared, and no time here is a promise.
//!
//! Prints the adapter, then per grid the compute pass's GPU time, the copy, the wall time of the
//! whole evolve, memory and set-up cost (`WaterRenderer::measure_ocean_cost`), and the worst
//! difference between the maps and the CPU reference FFT on this driver. The grids run in turns,
//! `--rounds` times, on one device, so a load that drifts hits each grid alike; the table gives the
//! medians over the rounds. With `--size`, it also times whole frames per grid against the swell
//! (the stand-in, what the frame draws with no compute), by the frame's own GPU timers: on a loaded
//! host the CPU's wall time hides a difference this small, so wall time is printed only beside them.
//!
//! ```text
//! REGIA_GPU_UUID=GPU-... REGIA_GPU_STRICT=1 cargo run --release -p regia_terrain --example water_ocean_cost -- \
//!     [--grids 256,512,1024] [--frames 64] [--rounds 5] [--size 1920x1080] [--frame-count 16] \
//!     [--foam-decay 2.0] [--whitecap-rescale]
//! ```
//!
//! `--foam-decay` (seconds) turns on lingering foam and `--whitecap-rescale` the whitecap rescale
//! (ADR-2843) for every row, so a run with and a run without give their cost on the card.

#[path = "../tests/water_scene/mod.rs"]
mod water_scene;

use std::error::Error;
use std::sync::OnceLock;
use std::time::Instant;

use regia_render::{FrameRenderer, RenderDevice};
use regia_terrain::water_body::ocean::{OceanFrame, loop_fraction, tables_for};
use regia_terrain::{FftOcean, WaterBody, WaterClock, WaterLook, WaterRenderer, WaterTier};
use water_scene::{Camera, STUDIO_SKY, frame, post, stream_scene};

const SUN: [f32; 3] = [0.35, -0.65, -0.55];
const LEVEL: f32 = -0.1;
const BOAT: Camera = Camera {
    eye: [-20.0, 4.0, 0.0],
    target: [-120.0, LEVEL, -10.0],
    fov_y: 0.9,
};

/// `--foam-decay` and `--whitecap-rescale`, set once by `main`.
static FOAM: OnceLock<(f32, bool)> = OnceLock::new();

struct Args {
    foam_decay: f32,
    whitecap_rescale: bool,
    grids: Vec<u32>,
    frames: u32,
    rounds: u32,
    size: Option<(u32, u32)>,
    frame_count: u32,
}

fn parse_args() -> Result<Args, Box<dyn Error>> {
    let mut args = Args {
        foam_decay: 0.0,
        whitecap_rescale: false,
        grids: vec![256, 512, 1024],
        frames: 64,
        rounds: 5,
        size: None,
        frame_count: 16,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--grids" => {
                args.grids = value()?
                    .split(',')
                    .map(str::parse::<u32>)
                    .collect::<Result<_, _>>()?;
            }
            "--frames" => args.frames = value()?.parse()?,
            "--rounds" => args.rounds = value()?.parse()?,
            "--frame-count" => args.frame_count = value()?.parse()?,
            "--foam-decay" => args.foam_decay = value()?.parse()?,
            "--whitecap-rescale" => args.whitecap_rescale = true,
            "--size" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or("--size needs WxH")?;
                args.size = Some((w.parse()?, h.parse()?));
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    Ok(args)
}

fn ocean(grid: u32) -> FftOcean {
    FftOcean {
        wind_speed: 14.0,
        wind_direction: [-1.0, -0.25],
        seed: 11,
        grid,
        foam_decay: FOAM.get().map_or(0.0, |f| f.0),
        whitecap_rescale: FOAM.get().is_some_and(|f| f.1),
        ..FftOcean::default()
    }
}

fn sea(grid: u32) -> WaterBody {
    let mut sea = WaterBody::plane([0.0, LEVEL, 0.0], [2000.0, 2000.0], WaterLook::ocean());
    sea.ocean = Some(ocean(grid));
    sea
}

/// The middle of `values`, for a table.
fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

/// The compute pass, grid by grid in turns.
fn pass_table(water: &WaterRenderer, args: &Args) -> Result<(), Box<dyn Error>> {
    let mut rows: Vec<Vec<regia_terrain::water_body::OceanCost>> = vec![Vec::new(); args.grids.len()];
    for round in 0..args.rounds {
        for (row, grid) in rows.iter_mut().zip(&args.grids) {
            match water.measure_ocean_cost(&ocean(0), *grid, args.frames)? {
                Some(cost) => {
                    println!(
                        "round {round} grid {grid}: compute {:?} ms, copy {:?} ms, wall {:.3} ms",
                        cost.compute_ms, cost.copy_ms, cost.submit_ms
                    );
                    row.push(cost);
                }
                None => println!("round {round} grid {grid}: this device cannot run it"),
            }
        }
    }
    println!(
        "grid | dispatches | modes | GPU MB | history MB | compute ms | copy ms | evolve wall ms | tables ms | upload ms"
    );
    for (row, grid) in rows.iter().zip(&args.grids) {
        let Some(first) = row.first() else { continue };
        let of = |f: fn(&regia_terrain::water_body::OceanCost) -> f64| median(row.iter().map(f).collect());
        println!(
            "{grid} | {} | {} | {:.0} | {:.1} | {:.3} | {:.3} | {:.3} | {:.1} | {:.1}",
            first.dispatches,
            first.modes,
            first.gpu_bytes as f64 / 1_048_576.0,
            first.history_bytes as f64 / 1_048_576.0,
            of(|c| c.compute_ms.unwrap_or(f64::NAN)),
            of(|c| c.copy_ms.unwrap_or(f64::NAN)),
            of(|c| c.submit_ms),
            of(|c| c.tables_ms),
            of(|c| c.upload_ms),
        );
    }
    Ok(())
}

/// The maps against the CPU reference on this driver, and the reference's own time.
fn reference_table(water: &WaterRenderer, args: &Args) -> Result<(), Box<dyn Error>> {
    println!("grid | tick | worst field error (m) | peak height (m) | CPU reference ms");
    for grid in &args.grids {
        let sea = sea(*grid);
        water.set_bodies(&[&sea]);
        water.set_tier(WaterTier::High);
        for tick in [240u64, 86_400] {
            let clock = WaterClock {
                tick,
                ..WaterClock::default()
            };
            water.set_clock(clock);
            let Some(maps) = water.read_ocean_maps()? else {
                println!("{grid} | {tick} | not drawn");
                continue;
            };
            let started = Instant::now();
            let cpu = OceanFrame::at(
                &tables_for(&maps.ocean, maps.size),
                loop_fraction(clock.seconds()),
            );
            let cpu_ms = started.elapsed().as_secs_f64() * 1000.0;
            let (mut worst, mut peak) = (0.0f32, 0.0f32);
            for (b, g) in cpu.grids.iter().enumerate() {
                let fields = [&g.dx, &g.h, &g.dz, &g.dxz, &g.hx, &g.hz, &g.dxx, &g.dzz];
                for row in 0..maps.size {
                    for column in 0..maps.size {
                        let i = (row * maps.size + column) as usize;
                        let (t0, t1) = (maps.texel(b, 0, column, row), maps.texel(b, 1, column, row));
                        for (field, gpu) in fields.iter().zip(t0.into_iter().chain(t1)) {
                            worst = worst.max((field[i] - gpu).abs());
                        }
                        peak = peak.max(t0[1].abs());
                    }
                }
            }
            println!("{grid} | {tick} | {worst:.2e} | {peak:.2} | {cpu_ms:.1}");
        }
    }
    Ok(())
}

/// One mode's frames: the frame's GPU timers and the wall time (ms).
#[derive(Default)]
struct FrameTimes {
    frame: Vec<f64>,
    scene: Vec<f64>,
    post: Vec<f64>,
    wall: Vec<f64>,
}

/// Whole frames per grid against the swell, in turns.
fn frame_table(device: &RenderDevice, args: &Args, size: (u32, u32)) -> Result<(), Box<dyn Error>> {
    let scene = stream_scene();
    let post = post();
    // The swell first (no maps), then the tier's own grid, then the grids asked for.
    let mut modes: Vec<(String, u32, bool)> =
        vec![("swell".to_string(), 0, false), ("tier 128".to_string(), 0, true)];
    modes.extend(args.grids.iter().map(|g| (format!("grid {g}"), *g, true)));
    let mut setups = Vec::new();
    for (name, grid, draw) in &modes {
        let mut renderer = FrameRenderer::new();
        renderer.set_studio_sky(device, Some(STUDIO_SKY));
        renderer.set_gpu_timing(device, true);
        let water = WaterRenderer::new(device);
        water.set_tier(WaterTier::High);
        let body = sea(*grid);
        water.set_bodies(&[&body]);
        water.set_ocean_draw(*draw);
        water.set_ambient_lux(18_000.0);
        renderer.set_translucent_layers(vec![water.layer()]);
        renderer
            .register_pass(water.ocean_pass())
            .map_err(|e| e.to_string())?;
        // Warm: pipelines, tables and maps do not count.
        for tick in 0..4 {
            water.set_clock(WaterClock {
                tick,
                ..WaterClock::default()
            });
            renderer.render_offscreen(device, &frame(&scene, BOAT, size.0, size.1, &post, SUN))?;
        }
        setups.push((name.clone(), renderer, water, FrameTimes::default()));
    }
    for round in 0..args.rounds {
        for (_, renderer, water, times) in &mut setups {
            for i in 0..args.frame_count {
                water.set_clock(WaterClock {
                    tick: 600 + u64::from(round) * 100 + u64::from(i),
                    ..WaterClock::default()
                });
                let started = Instant::now();
                renderer.render_offscreen(device, &frame(&scene, BOAT, size.0, size.1, &post, SUN))?;
                times.wall.push(started.elapsed().as_secs_f64() * 1000.0);
                let gpu = renderer.gpu_timings();
                times.frame.push(f64::from(gpu.frame_ms));
                times.scene.push(f64::from(gpu.scene_ms));
                times.post.push(f64::from(gpu.post_ms));
            }
        }
    }
    println!(
        "frame {}x{}, {} frames in {} turns, GPU timers (wall includes the readback):",
        size.0, size.1, args.frame_count, args.rounds
    );
    println!("mode | maps | GPU frame ms | against the swell | scene ms | post + water ms | wall ms");
    let mut base = None;
    for (name, _, water, times) in setups {
        let ms = median(times.frame);
        let base_ms = *base.get_or_insert(ms);
        println!(
            "{name} | {} | {ms:.3} | {:+.3} | {:.3} | {:.3} | {:.2}",
            water.stats().ocean_size,
            ms - base_ms,
            median(times.scene),
            median(times.post),
            median(times.wall),
        );
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = parse_args()?;
    let _ = FOAM.set((args.foam_decay, args.whitecap_rescale));
    let device = RenderDevice::headless()?;
    let info = device.adapter.get_info();
    println!(
        "adapter: {} ({:?}, driver {} {})",
        info.name, info.backend, info.driver, info.driver_info
    );
    println!(
        "ocean: Hs {:.2} m, peak {:.0} m, {} evolves per grid, {} turns",
        ocean(0).expected_height(),
        ocean(0).peak_wavelength(),
        args.frames,
        args.rounds
    );
    let water = WaterRenderer::new(&device);
    pass_table(&water, &args)?;
    reference_table(&water, &args)?;
    if let Some(size) = args.size {
        frame_table(&device, &args, size)?;
    }
    Ok(())
}
