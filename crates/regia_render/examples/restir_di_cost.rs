// SPDX-License-Identifier: MPL-2.0
//! Cost of the many-light sampler on a card (ADR-2470): the synthetic scene at a given size and
//! light count, the spatiotemporal estimate against the brute-force loop over each pixel's
//! cluster list (what the forward pass does today), timed per frame with a blocking wait after
//! each submit (GPU plus submit overhead; an upper bound on the passes' GPU time).
//!
//! ```text
//! cargo run --release -p regia_render --example restir_di_cost -- [width] [height] [lights] [frames]
//! ```
//!
//! Defaults: 1920 1080 1000 60. Select the card with `REGIA_GPU_INDEX` / `REGIA_GPU_NAME`.

use std::time::Instant;

use regia_render::restir_di::cpu::ClusterLists;
use regia_render::restir_di::scene::SyntheticScene;
use regia_render::restir_di::{ManyLight, RestirClusters, RestirDiGpu, RestirDiSettings, RestirInputs};
use regia_render::{RenderDevice, TrackedDevice};

fn arg(i: usize, default: u32) -> u32 {
    std::env::args()
        .nth(i)
        .and_then(|a| a.parse().ok())
        .unwrap_or(default)
}

fn texture(
    device: &RenderDevice,
    size: (u32, u32),
    format: wgpu::TextureFormat,
    texel: u32,
    bytes: &[u8],
) -> wgpu::Texture {
    let t = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("restir cost input"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    device.queue.write_texture(
        t.as_image_copy(),
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size.0 * texel),
            rows_per_image: Some(size.1),
        },
        wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
    );
    t
}

fn storage(device: &RenderDevice, bytes: &[u8]) -> wgpu::Buffer {
    let b = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("restir cost storage"),
        size: (bytes.len() as u64).max(64),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    device.queue.write_buffer(&b, 0, bytes);
    b
}

fn main() {
    let (w, h, n, frames) = (arg(1, 1920), arg(2, 1080), arg(3, 1000), arg(4, 60).max(4));
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("no GPU adapter: {err}");
            std::process::exit(2);
        }
    };
    println!("adapter: {}", device.adapter.get_info().name);
    let scene = SyntheticScene::new(w, h, n, 7);
    let clusters = ClusterLists::build(&scene.camera, w as f32 / h as f32, &scene.lights, 256);
    let (mean, max) = clusters.occupancy();
    println!(
        "{w}x{h}, {n} lights; cluster lists mean {mean:.1}, max {max}, overflowed {}",
        clusters.overflowed
    );
    let g = &scene.gbuffer;
    let normal: Vec<f32> = (0..g.len())
        .flat_map(|i| [g.normal[i][0], g.normal[i][1], g.normal[i][2], g.roughness[i]])
        .collect();
    let albedo: Vec<f32> = (0..g.len())
        .flat_map(|i| [g.albedo[i][0], g.albedo[i][1], g.albedo[i][2], g.metallic[i]])
        .collect();
    let motion = vec![0.0f32; g.len() * 4];
    let size = (w, h);
    let rgba = wgpu::TextureFormat::Rgba32Float;
    let textures = [
        texture(
            &device,
            size,
            wgpu::TextureFormat::R32Float,
            4,
            bytemuck::cast_slice(&g.depth),
        ),
        texture(&device, size, rgba, 16, bytemuck::cast_slice(&normal)),
        texture(&device, size, rgba, 16, bytemuck::cast_slice(&albedo)),
        texture(&device, size, rgba, 16, bytemuck::cast_slice(&motion)),
    ];
    let views = textures
        .each_ref()
        .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()));
    let lights = storage(&device, bytemuck::cast_slice::<ManyLight, u8>(&scene.lights));
    let counts = storage(&device, bytemuck::cast_slice(&clusters.counts));
    let indices = storage(&device, bytemuck::cast_slice(&clusters.indices));
    let tracked = TrackedDevice::observe(&device.device);
    for (label, reference) in [
        ("estimate (16 candidates, temporal, 4 spatial taps)", false),
        ("brute force over the list", true),
    ] {
        let mut sampler = match RestirDiGpu::new(&tracked, RestirDiSettings::default(), 1) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("{err}");
                std::process::exit(1);
            }
        };
        let inputs = RestirInputs {
            size,
            depth: &views[0],
            normal_roughness: &views[1],
            albedo_metallic: &views[2],
            motion: &views[3],
            lights: &lights,
            light_count: n,
            clusters: Some(RestirClusters {
                counts: &counts,
                indices: &indices,
                max_per_cluster: clusters.max_per_cluster,
            }),
            camera: scene.camera,
            reference,
        };
        let mut times = Vec::with_capacity(frames as usize);
        for f in 0..frames {
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("restir cost"),
                });
            if let Err(err) = sampler.record(&tracked, &device.queue, &mut encoder, &inputs) {
                eprintln!("{err}");
                std::process::exit(1);
            }
            let start = Instant::now();
            device.queue.submit([encoder.finish()]);
            let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
            if f >= 2 {
                times.push(start.elapsed().as_secs_f64() * 1e3);
            }
        }
        times.sort_by(f64::total_cmp);
        let median = times[times.len() / 2];
        let p90 = times[times.len() * 9 / 10];
        println!(
            "{label}: median {median:.3} ms, p90 {p90:.3} ms over {} frames",
            times.len()
        );
    }
}
