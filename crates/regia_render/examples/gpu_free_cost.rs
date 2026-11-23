// SPDX-License-Identifier: MPL-2.0
//! What freeing a big map's GPU objects costs the driver (the clean exit's renderer step,
//! ADR-188): creates `textures` mipmapped RGBA8 textures of `size`² and `buffers` 4 MiB buffers,
//! touches each once on the GPU, waits for idle, then drops them one by one on this thread and
//! again (recreated) spread over `threads` threads. Prints the times.
//!
//! `gpu_free_cost [textures 2000] [size 1024] [buffers 2000] [threads 4]`

use std::time::Instant;

use regia_render::RenderDevice;

fn arg(i: usize, default: u32) -> u32 {
    std::env::args()
        .nth(i)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Set {
    textures: Vec<wgpu::Texture>,
    views: Vec<wgpu::TextureView>,
    buffers: Vec<wgpu::Buffer>,
}

fn create(device: &RenderDevice, textures: u32, size: u32, buffers: u32) -> Set {
    let t = Instant::now();
    let mips = 32 - size.leading_zeros();
    let mut set = Set {
        textures: Vec::new(),
        views: Vec::new(),
        buffers: Vec::new(),
    };
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    for _ in 0..textures {
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("free cost texture"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // Committed and used once, as an uploaded texture is.
        encoder.clear_texture(&texture, &wgpu::ImageSubresourceRange::default());
        set.views
            .push(texture.create_view(&wgpu::TextureViewDescriptor::default()));
        set.textures.push(texture);
    }
    for _ in 0..buffers {
        let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("free cost buffer"),
            size: 4 << 20,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        encoder.clear_buffer(&buffer, 0, None);
        set.buffers.push(buffer);
    }
    device.queue.submit([encoder.finish()]);
    let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
    println!(
        "created {textures} textures ({size}², {mips} mips) and {buffers} buffers (4 MiB) in {:.0} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
    set
}

fn main() {
    let textures = arg(1, 2000);
    let size = arg(2, 1024);
    let buffers = arg(3, 2000);
    let threads = arg(4, 4).max(1) as usize;
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("no GPU adapter: {err}");
            std::process::exit(2);
        }
    };
    println!("adapter: {}", device.adapter.get_info().name);

    let set = create(&device, textures, size, buffers);
    let t = Instant::now();
    drop(set.views);
    let views = t.elapsed();
    let t = Instant::now();
    drop(set.textures);
    let tex = t.elapsed();
    let t = Instant::now();
    drop(set.buffers);
    let buf = t.elapsed();
    println!(
        "sequential: views {:.1} ms, textures {:.1} ms, buffers {:.1} ms",
        views.as_secs_f64() * 1e3,
        tex.as_secs_f64() * 1e3,
        buf.as_secs_f64() * 1e3
    );

    let mut set = create(&device, textures, size, buffers);
    let t = Instant::now();
    drop(std::mem::take(&mut set.views));
    let chunks = |n: usize| n.div_ceil(threads).max(1);
    let tex_chunk = chunks(set.textures.len());
    let buf_chunk = chunks(set.buffers.len());
    let mut texture_parts = Vec::new();
    while !set.textures.is_empty() {
        let at = set.textures.len().saturating_sub(tex_chunk);
        texture_parts.push(set.textures.split_off(at));
    }
    let mut buffer_parts = Vec::new();
    while !set.buffers.is_empty() {
        let at = set.buffers.len().saturating_sub(buf_chunk);
        buffer_parts.push(set.buffers.split_off(at));
    }
    std::thread::scope(|s| {
        for (textures, buffers) in texture_parts
            .into_iter()
            .zip(buffer_parts.into_iter().chain(std::iter::repeat_with(Vec::new)))
        {
            s.spawn(move || {
                drop(textures);
                drop(buffers);
            });
        }
    });
    println!(
        "{threads} threads: views + textures + buffers {:.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
}
