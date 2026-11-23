// SPDX-License-Identifier: MPL-2.0
//! The ETC2 / EAC blocks the cook writes decode on a real Vulkan driver to what the crate's own
//! decoder says (ADR-1379): the encoder and the decoder in `etc2_compress.rs` share the
//! specification's tables, so only a third-party decoder can catch a wrong table or bit order.
//!
//! A compute shader `textureLoad`s every texel of an ETC2 / EAC texture and packs it to RGBA8;
//! the result must equal `decode_etc2_rgba8` exactly for the 8-bit formats and within one step
//! for EAC R11 / RG11 (11-bit values rounded to 8). Needs `TEXTURE_COMPRESSION_ETC2`, which phone
//! GPUs have; Mesa's lavapipe on the build server and desktop NVIDIA do not (2026-10-22), and there the test
//! prints a skip and passes. Without such a GPU, ADR-1379 records the CPU cross-check against an
//! independent decoder (`texture2ddecoder`): ETC2 RGB8 and RGBA8 bit-exact, EAC through its alpha path.
//! The second test does the same for the full-mode `etc2_encode` module (T, H and planar too, ADR-2333).

use regia_asset_pipeline::etc2::{
    Etc2Kind, compress_eac_r11, compress_eac_rg11, compress_etc2_rgb8, compress_etc2_rgba8, decode_etc2_rgba8,
};
use regia_render::RenderDevice;
use wgpu::util::DeviceExt;

const SIZE: u32 = 16;

const SHADER: &str = r"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> dst: array<u32>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = textureDimensions(src);
    if (id.x >= size.x || id.y >= size.y) {
        return;
    }
    dst[id.y * size.x + id.x] = pack4x8unorm(textureLoad(src, vec2<i32>(id.xy), 0));
}
";

/// A deterministic 16×16 image with gradients, edges and an alpha ramp: every block differs.
fn image() -> Vec<u8> {
    let mut out = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let mut state = 0x2545_f491_u32;
    for y in 0..SIZE {
        for x in 0..SIZE {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let noise = (state & 0x1f) as u8;
            let edge = if (x / 3 + y / 5) % 2 == 0 { 0u8 } else { 90 };
            out.extend_from_slice(&[
                (x * 14) as u8 + noise,
                (y * 15) as u8,
                edge + noise,
                (x * 8 + y * 7) as u8,
            ]);
        }
    }
    out
}

/// Uploads `blocks` as a `format` texture, decodes it on the GPU and returns RGBA8 texels.
fn gpu_decode(device: &RenderDevice, format: wgpu::TextureFormat, blocks: &[u8]) -> Vec<u8> {
    gpu_decode_sized(device, format, SIZE, blocks)
}

/// [`gpu_decode`] for a `size × size` texture (`size` a multiple of 8).
fn gpu_decode_sized(device: &RenderDevice, format: wgpu::TextureFormat, size: u32, blocks: &[u8]) -> Vec<u8> {
    let d = &device.device;
    let texture = d.create_texture_with_data(
        &device.queue,
        &wgpu::TextureDescriptor {
            label: Some("etc2 decode src"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        },
        wgpu::util::TextureDataOrder::LayerMajor,
        blocks,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bytes = u64::from(size * size * 4);
    let storage = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("etc2 decode dst"),
        size: bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let staging = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("etc2 decode readback"),
        size: bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("etc2 decode"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("etc2 decode"),
        layout: None,
        module: &module,
        entry_point: Some("main"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("etc2 decode"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: storage.as_entire_binding(),
            },
        ],
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("etc2 decode"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(size / 8, size / 8, 1);
    }
    encoder.copy_buffer_to_buffer(&storage, 0, &staging, 0, bytes);
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    rx.recv().expect("map callback").expect("map");
    let data = staging
        .slice(..)
        .get_mapped_range()
        .expect("mapped range")
        .to_vec();
    staging.unmap();
    data
}

#[test]
fn etc2_and_eac_blocks_decode_on_the_gpu_like_the_crate_decoder() {
    let device = RenderDevice::headless().expect("headless GPU (lavapipe)");
    if !device
        .device
        .features()
        .contains(wgpu::Features::TEXTURE_COMPRESSION_ETC2)
    {
        eprintln!(
            "skip: {} has no TEXTURE_COMPRESSION_ETC2 (phone GPUs do)",
            device.adapter.get_info().name
        );
        return;
    }
    let rgba = image();
    let cases: [(Etc2Kind, wgpu::TextureFormat, Vec<u8>, u8); 4] = [
        (
            Etc2Kind::Rgb8,
            wgpu::TextureFormat::Etc2Rgb8Unorm,
            compress_etc2_rgb8(SIZE, SIZE, &rgba).expect("encode"),
            0,
        ),
        (
            Etc2Kind::Rgba8,
            wgpu::TextureFormat::Etc2Rgba8Unorm,
            compress_etc2_rgba8(SIZE, SIZE, &rgba).expect("encode"),
            0,
        ),
        (
            Etc2Kind::R11,
            wgpu::TextureFormat::EacR11Unorm,
            compress_eac_r11(SIZE, SIZE, &rgba).expect("encode"),
            1,
        ),
        (
            Etc2Kind::Rg11,
            wgpu::TextureFormat::EacRg11Unorm,
            compress_eac_rg11(SIZE, SIZE, &rgba).expect("encode"),
            1,
        ),
    ];
    for (kind, format, blocks, tolerance) in cases {
        let cpu = decode_etc2_rgba8(kind, SIZE, SIZE, &blocks).expect("cpu decode");
        let gpu = gpu_decode(&device, format, &blocks);
        let worst = cpu
            .iter()
            .zip(&gpu)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        let first_bad = cpu
            .as_chunks::<4>()
            .0
            .iter()
            .zip(gpu.as_chunks::<4>().0)
            .position(|(a, b)| a.iter().zip(b).any(|(x, y)| x.abs_diff(*y) > tolerance));
        assert!(
            worst <= tolerance,
            "{kind:?}: GPU and CPU decodes differ by {worst} (allowed {tolerance}); first texel {first_bad:?}: cpu {:?} gpu {:?}",
            first_bad.map(|i| &cpu[i * 4..i * 4 + 4]),
            first_bad.map(|i| &gpu[i * 4..i * 4 + 4]),
        );
    }
}

/// The full-mode encoder (`etc2_encode`, ADR-2333): random block words (every ETC2 RGB mode
/// occurs, about 6 % each for T, H and planar) and the all-mode encode of the test image decode
/// on the GPU exactly like the module's decoder. Skips without `TEXTURE_COMPRESSION_ETC2`.
#[test]
fn etc2_encode_blocks_of_every_mode_decode_on_the_gpu_like_the_module_decoder() {
    use regia_asset_pipeline::etc2_encode::{
        EncodeOptions, Etc2Format, RgbMode, classify_block, decode_image, encode_image,
    };
    let device = RenderDevice::headless().expect("headless GPU (lavapipe)");
    if !device
        .device
        .features()
        .contains(wgpu::Features::TEXTURE_COMPRESSION_ETC2)
    {
        eprintln!(
            "skip: {} has no TEXTURE_COMPRESSION_ETC2 (phone GPUs do)",
            device.adapter.get_info().name
        );
        return;
    }
    const WORDS: u32 = 64;
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut random = Vec::with_capacity((WORDS * WORDS / 16 * 8) as usize);
    for _ in 0..WORDS * WORDS / 16 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        random.extend_from_slice(&state.to_be_bytes());
    }
    let mut seen = [false; 5];
    for block in random.as_chunks::<8>().0 {
        seen[match classify_block(block) {
            RgbMode::Individual => 0,
            RgbMode::Differential => 1,
            RgbMode::T => 2,
            RgbMode::H => 3,
            RgbMode::Planar => 4,
        }] = true;
    }
    assert!(seen.iter().all(|s| *s), "random words cover every mode: {seen:?}");
    let rgba = image();
    let opts = EncodeOptions::default();
    let cases: [(u32, Etc2Format, wgpu::TextureFormat, Vec<u8>); 3] = [
        (
            WORDS,
            Etc2Format::Rgb8,
            wgpu::TextureFormat::Etc2Rgb8Unorm,
            random,
        ),
        (
            SIZE,
            Etc2Format::Rgb8,
            wgpu::TextureFormat::Etc2Rgb8Unorm,
            encode_image(SIZE, SIZE, &rgba, Etc2Format::Rgb8, &opts).expect("encode"),
        ),
        (
            SIZE,
            Etc2Format::Rgba8,
            wgpu::TextureFormat::Etc2Rgba8Unorm,
            encode_image(SIZE, SIZE, &rgba, Etc2Format::Rgba8, &opts).expect("encode"),
        ),
    ];
    for (size, format, wgpu_format, blocks) in cases {
        let cpu = decode_image(size, size, &blocks, format).expect("cpu decode");
        let gpu = gpu_decode_sized(&device, wgpu_format, size, &blocks);
        let first_bad = cpu
            .as_chunks::<4>()
            .0
            .iter()
            .zip(gpu.as_chunks::<4>().0)
            .position(|(a, b)| a != b);
        assert!(
            first_bad.is_none(),
            "{format:?} {size}x{size}: GPU and module decodes differ; first texel {first_bad:?}: cpu {:?} gpu {:?}",
            first_bad.map(|i| &cpu[i * 4..i * 4 + 4]),
            first_bad.map(|i| &gpu[i * 4..i * 4 + 4]),
        );
    }
}
