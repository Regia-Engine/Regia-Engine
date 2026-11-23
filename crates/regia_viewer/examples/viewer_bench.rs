// SPDX-License-Identifier: MPL-2.0
//! Frame-time measurement of the viewer on whatever adapter the environment exposes.
//!
//! ```text
//! cargo run --release -p regia_viewer --example viewer_bench -- [width] [height] [frames]
//! ```
//!
//! Prints the adapter, the mean and worst frame time while the view turns, the time of a material switch
//! (the switch itself and the first frame after it), and the counters.

use std::time::Instant;

use regia_viewer::gpu::{headless_device, sample_count_for};
use regia_viewer::standin::stud;
use regia_viewer::{
    GemParams, MetalParams, ViewerCamera, ViewerConfig, ViewerMaterial, ViewerRenderer, ViewerStyle,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<u32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let width = args.first().copied().unwrap_or(1080);
    let height = args.get(1).copied().unwrap_or(1080);
    let frames = args.get(2).copied().unwrap_or(120);

    let (adapter, device, queue) = headless_device()?;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let info = adapter.get_info();
    let mut config = ViewerConfig::new(format);
    config.sample_count = sample_count_for(&adapter, format);
    println!(
        "adapter: {} ({:?}), {width}x{height}, {}x MSAA",
        info.name, info.backend, config.sample_count
    );

    let created = Instant::now();
    let mut viewer = ViewerRenderer::new(device.clone(), queue, config)?;
    println!(
        "renderer created in {:.2} ms, {} pipelines",
        created.elapsed().as_secs_f64() * 1000.0,
        viewer.pipeline_count()
    );

    let mut first_metal = None;
    let mut first_stone = None;
    for part in stud() {
        let mesh = viewer.upload_mesh(&part.mesh)?;
        let material = viewer.create_material(&part.material);
        if matches!(part.material, ViewerMaterial::Metal(_)) && first_metal.is_none() {
            first_metal = Some(material);
        }
        if matches!(part.material, ViewerMaterial::Gem(_)) {
            first_stone = Some(material);
        }
        viewer.add_draw(mesh, material, &part.transforms)?;
    }
    viewer.set_style(&ViewerStyle::default());

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("bench target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });

    let base = ViewerCamera::front(36.0, 100.0, 36.0);
    let mut frame_ms = Vec::with_capacity(frames as usize);
    for frame in 0..frames {
        // The same small rock a drag makes, inside the +-30 degree tilt.
        let t = frame as f32 / frames.max(1) as f32 * std::f32::consts::TAU;
        let tilt = 25f32.to_radians() * t.sin();
        let turn = 25f32.to_radians() * (t * 2.0).sin();
        let eye = glam::Vec3::new(turn.sin() * tilt.cos(), tilt.sin(), turn.cos() * tilt.cos()) * 36.0;
        viewer.set_camera(&ViewerCamera {
            eye: eye.to_array(),
            ..base
        })?;
        let start = Instant::now();
        viewer.render(&target)?;
        device.poll(wgpu::PollType::wait_indefinitely())?;
        frame_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    // The first frame is reported on its own: a driver may finish compiling the pipelines there.
    let first = frame_ms.first().copied().unwrap_or(0.0);
    let mut rest: Vec<f64> = frame_ms.iter().skip(1).copied().collect();
    rest.sort_by(f64::total_cmp);
    if !rest.is_empty() {
        let mean = rest.iter().sum::<f64>() / rest.len() as f64;
        println!(
            "turning view: first frame {first:.2} ms; then mean {mean:.2} ms, p50 {:.2} ms, p99 {:.2} ms, worst {:.2} ms over {} frames",
            rest[rest.len() / 2],
            rest[(rest.len() * 99 / 100).min(rest.len() - 1)],
            rest[rest.len() - 1],
            rest.len()
        );
    }

    if let (Some(metal), Some(stone)) = (first_metal, first_stone) {
        let start = Instant::now();
        viewer.set_material(
            metal,
            &ViewerMaterial::Metal(MetalParams {
                base_color: [0.91, 0.92, 0.92],
                roughness: 0.1,
            }),
        )?;
        viewer.set_material(
            stone,
            &ViewerMaterial::Gem(GemParams {
                tint: [0.2, 0.3, 0.95],
                roughness: 0.02,
                ior: 1.77,
                dispersion: 0.2,
                sparkle: 0.6,
                seed: 1.0,
            }),
        )?;
        let switched = start.elapsed().as_secs_f64() * 1000.0;
        viewer.render(&target)?;
        device.poll(wgpu::PollType::wait_indefinitely())?;
        println!(
            "gem and metal switch: {switched:.3} ms to apply, {:.2} ms including the next frame",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    let stats = viewer.stats();
    println!("{stats:?}");
    Ok(())
}
