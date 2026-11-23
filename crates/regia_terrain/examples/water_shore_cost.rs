// SPDX-License-Identifier: MPL-2.0
//! What the shore bake costs (spec 87 § 9, ADR-1828): the swell sea over a beach with and without
//! its shore field, per tier, plus the bake itself and the buoyancy math.
//!
//! - Frame: wall time per offscreen frame (readback included) and, on a GPU with timestamp
//!   queries, `post_ms` (translucent layers + post stack), averaged over `--frames` frames after
//!   four warm-up frames, the tick advancing each frame. On lavapipe the numbers check that the
//!   path runs and what it costs on a CPU rasterizer only; real-GPU numbers come from the lead.
//! - Bake: one 1024 × 1024 field from an analytic bed, wall time.
//! - Buoyancy: `float_forces` for 100 floaters of 8 spheres on the swell sea, wall time per step,
//!   in deep water and across the beach, each with and without the field.
//!
//! ```text
//! cargo run --release -p regia_terrain --example water_shore_cost -- [--size 640x360] [--frames 16]
//! ```

#[path = "../tests/water_scene/mod.rs"]
mod water_scene;

use std::error::Error;
use std::sync::Arc;
use std::time::Instant;

use glam::{Quat, Vec3};
use regia_render::{FrameRenderer, RenderDevice};
use regia_terrain::water_body::float::{FloatPose, Floater, float_forces};
use regia_terrain::water_body::{ShoreBakeDesc, ShoreField};
use regia_terrain::{Swell, WaterBody, WaterClock, WaterLook, WaterRenderer, WaterTier};
use water_scene::{Camera, STUDIO_SKY, StreamScene, frame, post, stream_scene};

const SUN: [f32; 3] = [0.35, -0.65, -0.55];
const LEVEL: f32 = -0.1;
const VIEW: Camera = Camera {
    eye: [-6.0, 3.0, 4.0],
    target: [8.0, LEVEL, -1.0],
    fov_y: 0.9,
};

fn sea() -> WaterBody {
    let mut sea = WaterBody::plane([0.0, LEVEL, 0.0], [200.0, 200.0], WaterLook::ocean());
    sea.swell = Some(Swell::open_sea([1.0, 0.35], 0.35, 6.0, 20));
    sea
}

fn bed(x: f32, _z: f32) -> f32 {
    LEVEL + (x - 6.0) / 8.0
}

struct Args {
    size: (u32, u32),
    frames: u32,
}

fn args() -> Result<Args, Box<dyn Error>> {
    let mut out = Args {
        size: (640, 360),
        frames: 16,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--size" => {
                let v = it.next().ok_or("--size needs WxH")?;
                let (w, h) = v.split_once('x').ok_or("--size needs WxH")?;
                out.size = (w.parse()?, h.parse()?);
            }
            "--frames" => out.frames = it.next().ok_or("--frames needs a count")?.parse()?,
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    Ok(out)
}

/// Mean wall ms and `post_ms` per frame.
fn measure(
    device: &RenderDevice,
    scene: &StreamScene,
    body: &WaterBody,
    shore: Option<Arc<ShoreField>>,
    tier: WaterTier,
    args: &Args,
) -> Result<(f64, f32, u32), Box<dyn Error>> {
    let post = post();
    let mut renderer = FrameRenderer::new();
    renderer.set_studio_sky(device, Some(STUDIO_SKY));
    let timed = device.capabilities.timestamp_query;
    if timed {
        renderer.set_gpu_timing(device, true);
    }
    let water = WaterRenderer::new(device);
    water.set_tier(tier);
    water.set_bodies(&[body]);
    water.set_shores(&[shore]);
    water.set_ambient_lux(18_000.0);
    renderer.set_translucent_layers(vec![water.layer()]);
    let mut wall = 0.0f64;
    let mut post_ms = 0.0f32;
    for i in 0..(args.frames + 4) {
        water.set_clock(WaterClock {
            tick: 600 + u64::from(i),
            ..WaterClock::default()
        });
        let start = Instant::now();
        renderer.render_offscreen(device, &frame(scene, VIEW, args.size.0, args.size.1, &post, SUN))?;
        if i >= 4 {
            wall += start.elapsed().as_secs_f64() * 1000.0;
            if timed {
                post_ms += renderer.gpu_timings().post_ms;
            }
        }
    }
    let n = args.frames.max(1);
    Ok((
        wall / f64::from(n),
        post_ms / n as f32,
        water.stats().shore_fields,
    ))
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = args()?;
    // The bake: a 1024² field over 400 m.
    let start = Instant::now();
    let field = ShoreField::bake(
        &ShoreBakeDesc {
            level: LEVEL,
            min: [-200.0, -200.0],
            max: [200.0, 200.0],
            cell: 0.25,
        },
        bed,
    )?;
    let bake_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "bake: {}x{} texels, cell {:.3} m: {bake_ms:.1} ms",
        field.size[0], field.size[1], field.cell
    );
    let field = Arc::new(field);

    // Buoyancy: 100 floaters of 8 spheres on the swell sea, one step.
    let body = sea();
    let clock = WaterClock {
        tick: 600,
        ..WaterClock::default()
    };
    let floater = Floater::box_points([1.0, 0.4, 2.0], [2, 1, 4]);
    // Rows at x = -150..-123 sit in 18-20 m of water (deep for these waves), rows at x = -12..15
    // straddle the beach (shoaled, breaking, partly dry); each also without the field.
    for (label, x0) in [("deep", -150.0f32), ("beach", -12.0)] {
        for (with, shore) in [("field", Some(field.as_ref())), ("no field", None)] {
            let water = |x: f32, z: f32| body.surface_on(x, z, &clock, shore);
            let start = Instant::now();
            let rounds = 20;
            let mut checksum = 0.0f32;
            for _ in 0..rounds {
                for i in 0..100 {
                    let p = Vec3::new(x0 + 3.0 * (i % 10) as f32, LEVEL, -20.0 + 4.0 * (i / 10) as f32);
                    let pose = FloatPose {
                        position: p,
                        rotation: Quat::IDENTITY,
                        center_of_mass: p,
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    };
                    checksum += float_forces(&floater, &pose, water, 9.81).force[1];
                }
            }
            let float_ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(rounds);
            println!(
                "buoyancy ({label}, {with}): 100 floaters x 8 spheres: {float_ms:.3} ms per step (checksum {checksum:.1})"
            );
        }
    }

    let device = RenderDevice::headless()?;
    println!(
        "adapter: {}, {}x{}, {} frames, timestamps {}",
        device.adapter.get_info().name,
        args.size.0,
        args.size.1,
        args.frames,
        device.capabilities.timestamp_query
    );
    let scene = stream_scene();
    println!(
        "| tier | wall ms, no field | wall ms, field | post ms, no field | post ms, field | fields bound |"
    );
    println!("|---|---|---|---|---|---|");
    for tier in [WaterTier::Low, WaterTier::High, WaterTier::Ultra] {
        let (w0, p0, _) = measure(&device, &scene, &body, None, tier, &args)?;
        let (w1, p1, bound) = measure(&device, &scene, &body, Some(Arc::clone(&field)), tier, &args)?;
        println!(
            "| {} | {w0:.2} | {w1:.2} | {p0:.3} | {p1:.3} | {bound} |",
            tier.name()
        );
    }
    Ok(())
}
