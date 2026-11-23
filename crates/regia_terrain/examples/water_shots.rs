// SPDX-License-Identifier: MPL-2.0
//! Water evidence shots and GPU cost (ADR-1399).
//!
//! Renders the stream scene of `tests/water_scene` (a carved channel, pebble bed, three boulders,
//! a sloped ribbon of water) through `FrameRenderer` on the default GPU and writes PNGs: about
//! 2 m from the water (looking down the stream and straight down) and about 200 m away, at several
//! fixed-step ticks, per tier, with an ice shelf and against a low sun. With `--perf` it measures
//! the water's GPU cost at 1440p and 4K: the frame's `post_ms` (translucent layers + post stack)
//! with the water minus without it, averaged over the measured frames.
//!
//! ```text
//! cargo run --release -p regia_terrain --example water_shots -- <out dir> [--size 1920x1080] [--perf | --perf-only] [--frames 24]
//! ```

#[path = "../tests/water_scene/mod.rs"]
mod water_scene;

use std::error::Error;
use std::path::{Path, PathBuf};

use regia_render::{FrameRenderer, RenderDevice};
use regia_terrain::water_body::WaterDebug;
use regia_terrain::{IceShelf, WaterClock, WaterRenderer, WaterTier};
use water_scene::{Camera, FAR, NEAR, STUDIO_SKY, StreamScene, TOP, frame, post, stream_scene};

/// Sun travel directions: high from the side, and low (10°) straight ahead of the camera.
const SUN_SIDE: [f32; 3] = [0.35, -0.65, -0.55];
const SUN_LOW: [f32; 3] = [-0.98, -0.17, 0.1];

struct Args {
    out: PathBuf,
    size: (u32, u32),
    perf: bool,
    /// Skip the evidence shots (a slow device: lavapipe).
    perf_only: bool,
    frames: u32,
}

fn parse_args() -> Result<Args, Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mut out = None;
    let mut size = (1920, 1080);
    let mut perf = false;
    let mut perf_only = false;
    let mut frames = 24;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--perf" => perf = true,
            "--perf-only" => {
                perf = true;
                perf_only = true;
            }
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
        out: out.ok_or("usage: water_shots <out dir> [--size WxH] [--perf] [--frames N]")?,
        size,
        perf,
        perf_only,
        frames,
    })
}

#[derive(Clone, Copy)]
struct Shot {
    name: &'static str,
    camera: Camera,
    tier: Option<WaterTier>,
    tick: u64,
    ice: bool,
    sun: [f32; 3],
    debug: WaterDebug,
}

fn water_for(device: &RenderDevice, scene: &StreamScene, shot: &Shot) -> Option<WaterRenderer> {
    let tier = shot.tier?;
    let water = WaterRenderer::new(device);
    water.set_tier(tier);
    let mut stream = scene.stream.clone();
    if shot.ice {
        stream.ice = Some(IceShelf::default());
    }
    water.set_bodies(&[&stream]);
    water.set_ambient_lux(18_000.0);
    water.set_clock(WaterClock {
        tick: shot.tick,
        ..WaterClock::default()
    });
    water.set_debug(shot.debug);
    Some(water)
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

fn shots() -> Vec<Shot> {
    let base = Shot {
        name: "",
        camera: NEAR,
        tier: Some(WaterTier::High),
        tick: 0,
        ice: false,
        sun: SUN_SIDE,
        debug: WaterDebug::Off,
    };
    let mut list = vec![Shot {
        name: "near_bare",
        tier: None,
        ..base
    }];
    for (name, tick) in [
        ("near_high_t000", 0),
        ("near_high_t015", 15),
        ("near_high_t030", 30),
    ] {
        list.push(Shot { name, tick, ..base });
    }
    list.extend([
        Shot {
            name: "near_ultra_t000",
            tier: Some(WaterTier::Ultra),
            ..base
        },
        Shot {
            name: "near_low_t000",
            tier: Some(WaterTier::Low),
            ..base
        },
        Shot {
            name: "near_ice_t000",
            ice: true,
            ..base
        },
        Shot {
            name: "near_lowsun_t000",
            sun: SUN_LOW,
            ..base
        },
        Shot {
            name: "near_lowsun_t015",
            sun: SUN_LOW,
            tick: 15,
            ..base
        },
        Shot {
            name: "top_bare",
            camera: TOP,
            tier: None,
            ..base
        },
        Shot {
            name: "top_high_t000",
            camera: TOP,
            ..base
        },
        Shot {
            name: "top_high_t015",
            camera: TOP,
            tick: 15,
            ..base
        },
        Shot {
            name: "far_bare",
            camera: FAR,
            tier: None,
            ..base
        },
        Shot {
            name: "far_high_t000",
            camera: FAR,
            ..base
        },
        Shot {
            name: "far_high_t030",
            camera: FAR,
            tick: 30,
            ..base
        },
        Shot {
            name: "far_lowsun_t000",
            camera: FAR,
            sun: SUN_LOW,
            ..base
        },
        Shot {
            name: "far_low_t000",
            camera: FAR,
            tier: Some(WaterTier::Low),
            ..base
        },
    ]);
    for (name, debug) in [
        ("near_debug_transmitted", WaterDebug::Transmitted),
        ("near_debug_reflected", WaterDebug::Reflected),
        ("near_debug_normal", WaterDebug::Normal),
        ("near_debug_fresnel", WaterDebug::Fresnel),
        ("near_debug_masks", WaterDebug::Masks),
    ] {
        list.push(Shot { name, debug, ..base });
    }
    list
}

fn render_shots(device: &RenderDevice, scene: &StreamScene, args: &Args) -> Result<(), Box<dyn Error>> {
    let (w, h) = args.size;
    let post = post();
    let mut lines = Vec::new();
    for shot in shots() {
        let mut renderer = FrameRenderer::new();
        renderer.set_studio_sky(device, Some(STUDIO_SKY));
        let water = water_for(device, scene, &shot);
        if let Some(water) = &water {
            renderer.set_translucent_layers(vec![water.layer()]);
        }
        let pixels = renderer
            .render_offscreen(device, &frame(scene, shot.camera, w, h, &post, shot.sun))?
            .pixels;
        let again = {
            let mut renderer = FrameRenderer::new();
            renderer.set_studio_sky(device, Some(STUDIO_SKY));
            let water = water_for(device, scene, &shot);
            if let Some(water) = &water {
                renderer.set_translucent_layers(vec![water.layer()]);
            }
            renderer
                .render_offscreen(device, &frame(scene, shot.camera, w, h, &post, shot.sun))?
                .pixels
        };
        let (a, b) = (fnv1a(&pixels), fnv1a(&again));
        let line = format!(
            "{} {w}x{h} hash {a:016x} rerender {b:016x} {}",
            shot.name,
            if a == b { "stable" } else { "DIFFERS" }
        );
        println!("{line}");
        lines.push(line);
        write_png(&args.out.join(format!("{}.png", shot.name)), w, h, &pixels)?;
    }
    std::fs::write(args.out.join("hashes.txt"), lines.join("\n") + "\n")?;
    Ok(())
}

/// Mean `post_ms` and `frame_ms` over `frames` frames after a warm-up, the tick advancing each
/// frame.
fn measure(
    device: &RenderDevice,
    scene: &StreamScene,
    camera: Camera,
    size: (u32, u32),
    tier: Option<WaterTier>,
    frames: u32,
) -> Result<(f32, f32), Box<dyn Error>> {
    let post = post();
    let mut renderer = FrameRenderer::new();
    renderer.set_studio_sky(device, Some(STUDIO_SKY));
    renderer.set_gpu_timing(device, true);
    let shot = Shot {
        name: "",
        camera,
        tier,
        tick: 0,
        ice: false,
        sun: SUN_SIDE,
        debug: WaterDebug::Off,
    };
    let water = water_for(device, scene, &shot);
    if let Some(water) = &water {
        renderer.set_translucent_layers(vec![water.layer()]);
    }
    let (mut post_ms, mut frame_ms) = (0.0f32, 0.0f32);
    for i in 0..(frames + 4) {
        if let Some(water) = &water {
            water.set_clock(WaterClock {
                tick: u64::from(i),
                ..WaterClock::default()
            });
        }
        renderer.render_offscreen(device, &frame(scene, camera, size.0, size.1, &post, SUN_SIDE))?;
        if i >= 4 {
            let t = renderer.gpu_timings();
            post_ms += t.post_ms;
            frame_ms += t.frame_ms;
        }
    }
    let n = frames.max(1) as f32;
    Ok((post_ms / n, frame_ms / n))
}

fn perf(device: &RenderDevice, scene: &StreamScene, args: &Args) -> Result<(), Box<dyn Error>> {
    if !device.capabilities.timestamp_query {
        return Err("this GPU has no timestamp queries".into());
    }
    let mut lines = vec![
        format!("adapter: {}", device.adapter.get_info().name),
        "| size | view | tier | post ms | frame ms | water ms (post − no water) |".to_string(),
        "|---|---|---|---|---|---|".to_string(),
    ];
    for size in [(2560, 1440), (3840, 2160)] {
        for (view, camera) in [("2 m", NEAR), ("200 m", FAR)] {
            let (base_post, base_frame) = measure(device, scene, camera, size, None, args.frames)?;
            lines.push(format!(
                "| {}x{} | {view} | none | {base_post:.3} | {base_frame:.3} | — |",
                size.0, size.1
            ));
            for tier in [WaterTier::Low, WaterTier::High, WaterTier::Ultra] {
                let (p, f) = measure(device, scene, camera, size, Some(tier), args.frames)?;
                lines.push(format!(
                    "| {}x{} | {view} | {} | {p:.3} | {f:.3} | {:.3} |",
                    size.0,
                    size.1,
                    tier.name(),
                    p - base_post
                ));
            }
        }
    }
    for line in &lines {
        println!("{line}");
    }
    std::fs::write(args.out.join("perf.md"), lines.join("\n") + "\n")?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = parse_args()?;
    std::fs::create_dir_all(&args.out)?;
    let device = RenderDevice::headless()?;
    println!("adapter: {}", device.adapter.get_info().name);
    let scene = stream_scene();
    if !args.perf_only {
        render_shots(&device, &scene, &args)?;
    }
    if args.perf {
        perf(&device, &scene, &args)?;
    }
    Ok(())
}
