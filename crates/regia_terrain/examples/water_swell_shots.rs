// SPDX-License-Identifier: MPL-2.0
//! Swell evidence shots (ADR-1620).
//!
//! Renders a 2 km open sea with a deterministic swell (`Swell::open_sea`) beside the stream scene
//! of `tests/water_scene` through `FrameRenderer` on the default GPU and writes PNGs: from a boat
//! (3 m), from a cliff (40 m) and through a narrow scope (5° vertical field of view), at a few
//! fixed-step ticks, per tier, flat for comparison, and with the `normal` and `masks` debug views
//! (crest foam in red). Prints each frame's hash and the swell draw counts.
//!
//! With `--perf` (a GPU with timestamp queries) it measures the swell's GPU cost instead: per view
//! and tier, the frame's `post_ms` (translucent layers + post stack) with the swell sea minus with
//! the same sea flat, averaged over `--frames` frames (default 24) after four warm-up frames, the
//! tick advancing each frame; written to `<out dir>/perf.md`.
//!
//! ```text
//! cargo run --release -p regia_terrain --example water_swell_shots -- <out dir> [--size 1920x1080] [--perf] [--frames 24]
//! ```

#[path = "../tests/water_scene/mod.rs"]
mod water_scene;

use std::error::Error;
use std::path::{Path, PathBuf};

use regia_render::{FrameRenderer, RenderDevice};
use regia_terrain::water_body::WaterDebug;
use regia_terrain::{Swell, WaterBody, WaterClock, WaterLook, WaterRenderer, WaterTier};
use water_scene::{Camera, STUDIO_SKY, StreamScene, frame, post, stream_scene};

/// The sun's travel direction: about 40° up, from behind the camera's right.
const SUN: [f32; 3] = [0.35, -0.65, -0.55];
/// The sea level, just under the stream scene's banks.
const LEVEL: f32 = -0.1;

const BOAT: Camera = Camera {
    eye: [-20.0, 3.0, 0.0],
    target: [-120.0, LEVEL, -10.0],
    fov_y: 0.9,
};
const CLIFF: Camera = Camera {
    eye: [-10.0, 40.0, 30.0],
    target: [-200.0, LEVEL, -60.0],
    fov_y: 0.9,
};
const SCOPE: Camera = Camera {
    eye: [-10.0, 6.0, 0.0],
    target: [-600.0, LEVEL, 0.0],
    fov_y: 0.0873,
};

struct Shot {
    name: &'static str,
    camera: Camera,
    tier: WaterTier,
    tick: u64,
    swell: bool,
    debug: WaterDebug,
}

fn shots() -> Vec<Shot> {
    let mut list = Vec::new();
    for (name, camera) in [("boat", BOAT), ("cliff", CLIFF), ("scope", SCOPE)] {
        for tick in [600u64, 630, 660] {
            list.push(Shot {
                name,
                camera,
                tier: WaterTier::Ultra,
                tick,
                swell: true,
                debug: WaterDebug::Off,
            });
        }
        for tier in [WaterTier::Low, WaterTier::High] {
            list.push(Shot {
                name,
                camera,
                tier,
                tick: 600,
                swell: true,
                debug: WaterDebug::Off,
            });
        }
        for (swell, debug) in [
            (false, WaterDebug::Off),
            (true, WaterDebug::Normal),
            (true, WaterDebug::Masks),
        ] {
            list.push(Shot {
                name,
                camera,
                tier: WaterTier::Ultra,
                tick: 600,
                swell,
                debug,
            });
        }
    }
    list
}

struct Args {
    out: PathBuf,
    size: (u32, u32),
    perf: bool,
    frames: u32,
}

fn parse_args() -> Result<Args, Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mut out = None;
    let mut size = (1920, 1080);
    let mut perf = false;
    let mut frames = 24;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--perf" => perf = true,
            "--frames" => frames = args.next().ok_or("--frames needs a count")?.parse()?,
            "--size" => {
                let text = args.next().ok_or("--size needs WxH")?;
                let (w, h) = text.split_once('x').ok_or("--size needs WxH")?;
                size = (w.parse()?, h.parse()?);
            }
            other => out = Some(PathBuf::from(other)),
        }
    }
    Ok(Args {
        out: out.ok_or("usage: water_swell_shots <out dir> [--size WxH] [--perf] [--frames N]")?,
        size,
        perf,
        frames,
    })
}

/// Mean `post_ms` over `frames` frames of `body` after four warm-up frames, the tick advancing
/// each frame.
fn measure(
    device: &RenderDevice,
    scene: &StreamScene,
    view: (Camera, (u32, u32)),
    body: &WaterBody,
    tier: WaterTier,
    frames: u32,
) -> Result<f32, Box<dyn Error>> {
    let (camera, size) = view;
    let post = post();
    let mut renderer = FrameRenderer::new();
    renderer.set_studio_sky(device, Some(STUDIO_SKY));
    renderer.set_gpu_timing(device, true);
    let water = WaterRenderer::new(device);
    water.set_tier(tier);
    water.set_bodies(&[body]);
    water.set_ambient_lux(18_000.0);
    renderer.set_translucent_layers(vec![water.layer()]);
    let mut post_ms = 0.0f32;
    for i in 0..(frames + 4) {
        water.set_clock(WaterClock {
            tick: 600 + u64::from(i),
            ..WaterClock::default()
        });
        renderer.render_offscreen(device, &frame(scene, camera, size.0, size.1, &post, SUN))?;
        if i >= 4 {
            post_ms += renderer.gpu_timings().post_ms;
        }
    }
    Ok(post_ms / frames.max(1) as f32)
}

fn perf(
    device: &RenderDevice,
    scene: &StreamScene,
    sea: &WaterBody,
    args: &Args,
) -> Result<(), Box<dyn Error>> {
    if !device.capabilities.timestamp_query {
        return Err("this GPU has no timestamp queries".into());
    }
    let mut flat = sea.clone();
    flat.swell = None;
    let mut lines = vec![
        format!(
            "adapter: {}, {}x{}",
            device.adapter.get_info().name,
            args.size.0,
            args.size.1
        ),
        "| view | tier | flat post ms | swell post ms | swell ms (swell − flat) |".to_string(),
        "|---|---|---|---|---|".to_string(),
    ];
    for (name, camera) in [("boat", BOAT), ("cliff", CLIFF), ("scope", SCOPE)] {
        for tier in [WaterTier::Low, WaterTier::High, WaterTier::Ultra] {
            let view = (camera, args.size);
            let base = measure(device, scene, view, &flat, tier, args.frames)?;
            let waved = measure(device, scene, view, sea, tier, args.frames)?;
            lines.push(format!(
                "| {name} | {} | {base:.3} | {waved:.3} | {:.3} |",
                tier.name(),
                waved - base
            ));
        }
    }
    for line in &lines {
        println!("{line}");
    }
    std::fs::write(args.out.join("perf.md"), lines.join("\n") + "\n")?;
    Ok(())
}

fn write_png(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<(), Box<dyn Error>> {
    let tmp = path.with_extension("png.tmp");
    {
        let file = std::fs::File::create(&tmp)?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(pixels)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = parse_args()?;
    let (out, (width, height)) = (args.out.clone(), args.size);
    std::fs::create_dir_all(&out)?;
    let device = RenderDevice::headless()?;
    let scene = stream_scene();
    let mut sea = WaterBody::plane([0.0, LEVEL, 0.0], [1000.0, 1000.0], WaterLook::ocean());
    sea.swell = Some(Swell::open_sea([-1.0, 0.25], 1.4, 40.0, 24));
    if args.perf {
        return perf(&device, &scene, &sea, &args);
    }
    let mut flat = sea.clone();
    flat.swell = None;
    let post = post();
    for shot in shots() {
        let mut renderer = FrameRenderer::new();
        renderer.set_studio_sky(&device, Some(STUDIO_SKY));
        let water = WaterRenderer::new(&device);
        water.set_tier(shot.tier);
        water.set_debug(shot.debug);
        water.set_clock(WaterClock {
            tick: shot.tick,
            ..WaterClock::default()
        });
        water.set_bodies(&[if shot.swell { &sea } else { &flat }]);
        water.set_ambient_lux(18_000.0);
        renderer.set_translucent_layers(vec![water.layer()]);
        let pixels = renderer
            .render_offscreen(&device, &frame(&scene, shot.camera, width, height, &post, SUN))?
            .pixels;
        let label = format!(
            "{}_{}_t{}{}{}",
            shot.name,
            shot.tier.name(),
            shot.tick,
            if shot.swell { "" } else { "_flat" },
            if shot.debug == WaterDebug::Off {
                String::new()
            } else {
                format!("_{}", shot.debug.name())
            },
        );
        write_png(&out.join(format!("{label}.png")), width, height, &pixels)?;
        let stats = water.stats();
        println!(
            "{label}: hash {:016x}, waves {}, rings {}, cell {} m, half cells {}, ring triangles {}",
            fnv1a(&pixels),
            stats.swell_waves,
            stats.rings,
            stats.ring_cell_m,
            stats.ring_half_cells,
            stats.ring_triangles,
        );
    }
    Ok(())
}
