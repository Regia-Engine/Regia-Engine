// SPDX-License-Identifier: MPL-2.0
//! How this adapter decodes sRGB texels (lane radv-sampler-followup, ADR-5713).
//!
//! A fragment probe reads every 8-bit sRGB code through the three paths a material texture takes:
//! `textureLoad`, a linear sample at the texel centre, and a linear sample of a 1×1 texture (the
//! last mip of a chain, which the terrain's macro tint divides by). Each is compared with the
//! exact sRGB transfer function (IEC 61966-2-1) in f64. `REGIA_DIAG_OUT=<dir>` also writes the
//! table as CSV.
//!
//! Measured 2026-11-11: an RX 7900 XTX (RADV, Mesa 26.2.6), a Steam Deck (RADV VANGOGH, the same
//! table) and an RTX 5080 (615.78.08) return linear values with 8 significant bits (within 0.39 %
//! of exact from code 16 up, 0.54 % at most below), and AMD and NVIDIA round 29 of the 256 codes to
//! different neighbours (codes 1 to 12, 15, 19, 20, 32, 33, 65, 66, 68, 70, 97, 184, 186 and 249
//! to 253: 0.4 to 0.7 % apart); lavapipe returns full-precision values from a fitted curve (2.5 %
//! off at code 1, 2.1 % at code 16, 0.3 of an 8-bit linear step at most). A value that depends on
//! one texel's decode therefore differs between adapters by up to that much: `regia_terrain --test
//! photoreal macro_variation_modulates_albedo` reads a macro mean of codes (253, 253, 251), 0.9805
//! on RADV and 0.9844 on NVIDIA in red and green.

use regia_render::RenderDevice;

const PROBE: &str = r"
@group(0) @binding(0) var ramp: texture_2d<f32>;
@group(0) @binding(1) var ones: texture_2d_array<f32>;
@group(0) @binding(2) var samp: sampler;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}
@fragment fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let x = i32(pos.x);
    let load = textureLoad(ramp, vec2<i32>(x, 0), 0).r;
    let centre = textureSampleLevel(ramp, samp, vec2<f32>((f32(x) + 0.5) / 256.0, 0.5), 0.0).r;
    let one = textureSampleLevel(ones, samp, vec2<f32>(0.5), x, 0.0).r;
    // Halfway between texels x and x + 1: the filter's blend of two decoded texels.
    let mid = textureSampleLevel(ramp, samp, vec2<f32>((f32(x) + 1.0) / 256.0, 0.5), 0.0).r;
    return vec4<f32>(load, centre, one, mid);
}
";

/// The exact sRGB electro-optical transfer function of 8-bit code `code`.
fn srgb_to_linear(code: u32) -> f64 {
    let c = f64::from(code) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Per code: (load, centre sample, 1×1 sample, halfway sample).
fn probe(device: &RenderDevice) -> Vec<[f32; 4]> {
    let (d, q) = (&device.device, &device.queue);
    let srgb = wgpu::TextureFormat::Rgba8UnormSrgb;
    let texture = |label: &str, width: u32, layers: u32| {
        d.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height: 1,
                depth_or_array_layers: layers,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: srgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    };
    let ramp = texture("srgb probe ramp", 256, 1);
    let ones = texture("srgb probe ones", 1, 256);
    let codes: Vec<u8> = (0..=255u8).flat_map(|c| [c, c, c, 255]).collect();
    let write = |tex: &wgpu::Texture, layer: u32, data: &[u8], width: u32| {
        q.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: 0, z: layer },
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
    };
    write(&ramp, 0, &codes, 256);
    for (layer, texel) in codes.as_chunks::<4>().0.iter().enumerate() {
        write(&ones, layer as u32, texel, 1);
    }
    let samp = d.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let target = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("srgb probe target"),
        size: wgpu::Extent3d {
            width: 256,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("srgb probe"),
        source: wgpu::ShaderSource::Wgsl(PROBE.into()),
    });
    let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("srgb probe"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba32Float,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    });
    let ramp_view = ramp.create_view(&wgpu::TextureViewDescriptor::default());
    let ones_view = ones.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    });
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("srgb probe"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&ramp_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&ones_view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&samp),
            },
        ],
    });
    let row = 256 * 16;
    let read = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("srgb probe read"),
        size: u64::from(row),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let tview = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut enc = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("srgb probe"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &tview,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(0..3, 0..1);
    }
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &read,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(1),
            },
        },
        wgpu::Extent3d {
            width: 256,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    q.submit([enc.finish()]);
    read.slice(..)
        .map_async(wgpu::MapMode::Read, |r| r.expect("map the probe"));
    d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = read.slice(..).get_mapped_range().expect("mapped probe");
    (0..256usize)
        .map(|i| {
            let f = |c: usize| {
                let b = &data[i * 16 + 4 * c..i * 16 + 4 * c + 4];
                f32::from_le_bytes([b[0], b[1], b[2], b[3]])
            };
            [f(0), f(1), f(2), f(3)]
        })
        .collect()
}

#[test]
fn srgb_decode_probe_dump() {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip srgb_decode_probe: {err}");
            return;
        }
    };
    let info = device.adapter.get_info();
    let name = format!("{} ({} {})", info.name, info.driver, info.driver_info);
    let table = probe(&device);
    let exact: Vec<f64> = (0..256).map(srgb_to_linear).collect();
    for (path, label) in [
        "textureLoad",
        "linear sample at the centre",
        "linear sample of a 1x1 texture",
    ]
    .iter()
    .enumerate()
    {
        // Worst absolute error in 8-bit linear steps, and worst relative error above code 16.
        let (mut abs_at, mut abs_max, mut rel_at, mut rel_max) = (0usize, 0.0f64, 0usize, 0.0f64);
        for (code, row) in table.iter().enumerate() {
            let err = f64::from(row[path]) - exact[code];
            if err.abs() > abs_max.abs() {
                (abs_at, abs_max) = (code, err);
            }
            if code >= 16 && (err / exact[code]).abs() > rel_max.abs() {
                (rel_at, rel_max) = (code, err / exact[code]);
            }
        }
        // Significant bits of the returned values: 24 minus the mantissa's trailing zeros.
        let bits = table
            .iter()
            .map(|row| row[path].to_bits())
            .filter(|b| b & 0x7fff_ffff != 0)
            .map(|b| 24 - ((b & 0x007f_ffff) | 0x0080_0000).trailing_zeros())
            .max()
            .unwrap_or(0);
        eprintln!(
            "{name}: {label}: worst error {:+.4} of an 8-bit linear step at code {abs_at}, worst \
             relative error {:+.4} % at code {rel_at}, at most {bits} significant bits",
            abs_max * 255.0,
            rel_max * 100.0
        );
    }
    // The halfway sample against the mean of the two exact neighbours.
    let (mut at, mut max) = (0usize, 0.0f64);
    for code in 16..255usize {
        let want = 0.5 * (exact[code] + exact[code + 1]);
        let rel = (f64::from(table[code][3]) - want) / want;
        if rel.abs() > max.abs() {
            (at, max) = (code, rel);
        }
    }
    eprintln!(
        "{name}: halfway between two texels: worst relative error {:+.4} % at code {at}",
        max * 100.0
    );
    if let Ok(dir) = std::env::var("REGIA_DIAG_OUT") {
        let mut csv = String::from("code,exact,load,centre,one,mid\n");
        for (code, row) in table.iter().enumerate() {
            csv.push_str(&format!(
                "{code},{:.9},{:.9},{:.9},{:.9},{:.9}\n",
                exact[code], row[0], row[1], row[2], row[3]
            ));
        }
        std::fs::write(std::path::Path::new(&dir).join("srgb_decode_probe.csv"), csv)
            .expect("write the table");
    }
    // The three paths agree (one decoder), and every code decodes within three 8-bit linear steps
    // of the exact curve: a sanity bound only (measured at most 0.96 of a step; a decode that was
    // skipped would be off by up to 67 steps).
    for (code, row) in table.iter().enumerate() {
        assert!(
            row[0].to_bits() == row[1].to_bits() && row[0].to_bits() == row[2].to_bits(),
            "code {code}: textureLoad {}, the centre sample {} and the 1x1 sample {} differ",
            row[0],
            row[1],
            row[2]
        );
        for value in &row[..3] {
            assert!(
                (f64::from(*value) - exact[code]).abs() * 255.0 < 3.0,
                "code {code} decodes to {value}, exact {:.6}",
                exact[code]
            );
        }
    }
}
