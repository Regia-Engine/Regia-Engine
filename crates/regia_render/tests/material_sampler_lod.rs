// SPDX-License-Identifier: MPL-2.0
//! The material sampler's trilinear level on this adapter (lane radv-sampler-followup).
//!
//! A fragment probe samples a texture whose mip `k` holds the value `k` (RGBA16F), so a sample's
//! red channel is the level the adapter blended to. Each 8×8 block of the target draws one
//! isotropic footprint (`s` level-0 texels per pixel, implicit derivatives, an optional mip bias,
//! as `sample_material` samples the forward path) through an isotropic trilinear sampler, the
//! 8x material sampler and a 16x one.

use regia_render::RenderDevice;

/// Serialises device creation and teardown across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One block per query: `q.x` = level-0 texels per pixel, `q.y` = mip bias.
/// Output: r = isotropic sampler, g = 8x, b = 16x, a = 8x through `textureSampleGrad`.
const PROBE: &str = r"
struct Qs { q: array<vec4<f32>, 64> };
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var s_iso: sampler;
@group(0) @binding(2) var s_8: sampler;
@group(0) @binding(3) var s_16: sampler;
@group(0) @binding(4) var<uniform> qs: Qs;
@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}
@fragment fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let block = min(u32(pos.x) / 8u, 63u);
    let q = qs.q[block];
    let size = vec2<f32>(textureDimensions(tex, 0));
    let uv = (pos.xy * q.x) / size + vec2<f32>(0.37, 0.61);
    let iso = textureSampleBias(tex, s_iso, uv, q.y);
    let a8 = textureSampleBias(tex, s_8, uv, q.y);
    let a16 = textureSampleBias(tex, s_16, uv, q.y);
    let k = exp2(q.y);
    let g8 = textureSampleGrad(tex, s_8, uv, dpdx(uv) * k, dpdy(uv) * k);
    return vec4<f32>(iso.r, a8.r, a16.r, g8.r);
}
";

const PROBE_SIZE: u32 = 256;
const LEVELS: u32 = 9;

/// Per query: (isotropic, 8x, 16x, 8x grad) blended levels.
fn probe(device: &RenderDevice, queries: &[(f32, f32)]) -> Vec<[f32; 4]> {
    let (d, q) = (&device.device, &device.queue);
    assert!(queries.len() <= 64);
    let tex = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("material lod probe"),
        size: wgpu::Extent3d {
            width: PROBE_SIZE,
            height: PROBE_SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: LEVELS,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for k in 0..LEVELS {
        let s = (PROBE_SIZE >> k).max(1);
        let v = half_bits(k as f32);
        let one = half_bits(1.0);
        let px: Vec<u8> = (0..s * s)
            .flat_map(|_| [v, v, v, one])
            .flat_map(u16::to_le_bytes)
            .collect();
        q.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: k,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &px,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(8 * s),
                rows_per_image: Some(s),
            },
            wgpu::Extent3d {
                width: s,
                height: s,
                depth_or_array_layers: 1,
            },
        );
    }
    let sampler = |anisotropy_clamp: u16| {
        d.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp,
            ..Default::default()
        })
    };
    let (s_iso, s_8, s_16) = (sampler(1), sampler(8), sampler(16));
    let mut qbytes = vec![0u8; 64 * 16];
    for (i, &(s, bias)) in queries.iter().enumerate() {
        qbytes[i * 16..i * 16 + 4].copy_from_slice(&s.to_le_bytes());
        qbytes[i * 16 + 4..i * 16 + 8].copy_from_slice(&bias.to_le_bytes());
    }
    let ubuf = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("material lod probe queries"),
        size: qbytes.len() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    q.write_buffer(&ubuf, 0, &qbytes);
    let width = 8 * queries.len() as u32;
    let target = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("material lod probe target"),
        size: wgpu::Extent3d {
            width,
            height: 8,
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
        label: Some("material lod probe"),
        source: wgpu::ShaderSource::Wgsl(PROBE.into()),
    });
    let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("material lod probe"),
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
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("material lod probe"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&s_iso),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&s_8),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::Sampler(&s_16),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: ubuf.as_entire_binding(),
            },
        ],
    });
    let row = (width * 16).div_ceil(256) * 256;
    let read = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("material lod probe read"),
        size: u64::from(row) * 8,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let tview = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut enc = d.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("material lod probe"),
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
                rows_per_image: Some(8),
            },
        },
        wgpu::Extent3d {
            width,
            height: 8,
            depth_or_array_layers: 1,
        },
    );
    q.submit([enc.finish()]);
    read.slice(..)
        .map_async(wgpu::MapMode::Read, |r| r.expect("map the probe"));
    d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = read.slice(..).get_mapped_range().expect("mapped probe");
    (0..queries.len())
        .map(|i| {
            // Pixel (8 i + 4, 4): a block's centre.
            let at = 4 * row as usize + (8 * i + 4) * 16;
            let f = |c: usize| {
                let b = &data[at + 4 * c..at + 4 * c + 4];
                f32::from_le_bytes([b[0], b[1], b[2], b[3]])
            };
            [f(0), f(1), f(2), f(3)]
        })
        .collect()
}

/// IEEE half bits of a small non-negative integer-valued `v` (exact for 0..=8).
fn half_bits(v: f32) -> u16 {
    if v == 0.0 {
        return 0;
    }
    let bits = v.to_bits();
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mant = (bits >> 13) & 0x3ff;
    ((exp as u32) << 10 | mant) as u16
}

#[test]
fn material_sampler_lod_dump() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let info = device.adapter.get_info();
    let mut queries = Vec::new();
    for s in [
        1.0f32, 1.05, 1.1, 1.18, 1.25, 1.33, 1.41, 1.5, 1.6, 1.68, 1.75, 1.87, 2.0, 2.2, 2.39, 2.6, 3.0, 4.48,
    ] {
        queries.push((s, 0.0f32));
    }
    // The tsr_motion plane: 1 / 1.6 / 3 texels per output pixel at render scale 0.67, TSR bias.
    for density in [1.0f32, 1.6, 3.0] {
        queries.push((density / 0.67, -0.680_461_2));
    }
    let lods = probe(&device, &queries);
    for (&(s, bias), l) in queries.iter().zip(&lods) {
        let exact = (s.log2() + bias).max(0.0);
        eprintln!(
            "{} ({} {}): {s:.3} texels/px bias {bias:.3}: exact {exact:.3} iso {:.3} 8x {:.3} 16x {:.3} 8x-grad {:.3}",
            info.name, info.driver, info.driver_info, l[0], l[1], l[2], l[3]
        );
    }
}

/// The material sampler's clamp (`frame::pipelines::MATERIAL_ANISOTROPY`).
const MATERIAL_ANISOTROPY: u16 = 8;

/// ADR-5670: with the device's calibrated offset added to the bias (as `sample_material` does
/// through `frame.fill_color.w`), the 8x material sampler never filters an isotropic footprint
/// sharper than the exact trilinear level the isotropic sampler reads, with and without the TSR
/// bias. Before the calibration RADV (RX 7900 XTX, Mesa 26.2.6) read level 0.000 for an exact 0.239
/// and 1.016 for 1.257 (`material_sampler_lod_dump`), and `REGIA_MATERIAL_ANISO_LOD_OFFSET=0`
/// reproduces that here.
#[test]
fn the_material_sampler_is_never_sharper_than_trilinear() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().unwrap_or_else(|err| panic!("no GPU: {err}"));
    let info = device.adapter.get_info();
    let offset =
        regia_render::material_anisotropy_lod_offset(&device.device, &device.queue, MATERIAL_ANISOTROPY);
    assert!(
        (0.0..=regia_render::material_aniso_calibration::MAX_MATERIAL_ANISO_LOD_OFFSET).contains(&offset),
        "offset {offset}"
    );
    assert_eq!(regia_render::last_material_anisotropy_lod_offset(), Some(offset));
    let mut queries = Vec::new();
    for s in [
        1.05f32, 1.1, 1.18, 1.25, 1.33, 1.41, 1.5, 1.6, 1.75, 2.2, 2.39, 2.6, 3.0, 4.48,
    ] {
        queries.push((s, 0.0f32));
    }
    for density in [1.0f32, 1.6, 3.0] {
        queries.push((density / 0.67, -0.680_461_2));
    }
    let biased: Vec<(f32, f32)> = queries.iter().map(|&(s, b)| (s, b + offset)).collect();
    let lods = probe(&device, &biased);
    for (&(s, bias), l) in queries.iter().zip(&lods) {
        let exact = (s.log2() + bias).max(0.0);
        let (iso, aniso_bias, aniso_grad) = (l[0], l[1], l[3]);
        eprintln!(
            "{} ({} {}): {s:.3} texels/px, bias {bias:.3} + offset {offset:.3}: exact {exact:.3}, \
             isotropic {iso:.3}, 8x {aniso_bias:.3} (grad {aniso_grad:.3})",
            info.name, info.driver, info.driver_info
        );
        // Filter-weight rounding: lavapipe reads up to ~0.04 under exact.
        for (path, level) in [
            ("textureSampleBias", aniso_bias),
            ("textureSampleGrad", aniso_grad),
        ] {
            assert!(
                level >= exact - 0.06,
                "{} ({} {}): the 8x material sampler filters a {s:.3}-texel footprint (bias {bias:.3}) at level \
                 {level:.3} through {path} with the calibrated offset {offset:.3}, sharper than the exact {exact:.3}",
                info.name,
                info.driver,
                info.driver_info
            );
        }
    }
}
