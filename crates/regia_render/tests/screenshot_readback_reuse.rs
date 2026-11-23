// SPDX-License-Identifier: MPL-2.0
//! ADR-8848 (ADR-8782 follow-up): a window screenshot's readback reuses its buffers. Each capture
//! used to create a mappable GPU buffer and a frame-sized RGBA8 vector; glibc kept the freed
//! vectors, and the Steam Deck grew about 10 MB per shot. Now a same-size capture copies into the
//! window's kept readback buffer ([`ReadbackBuffers`]) and its pixels land in a pooled vector
//! ([`screenshot::FRAME_BUFFERS`]) that the consumer gives back ([`screenshot::recycle`]).
//!
//! On lavapipe: one Deck-sized (1280 x 800) texture read back several times. The binary installs
//! the shared counting allocator (ADR-3291) and reads this thread's requests. No wall clock.

use regia_render::RenderDevice;
use regia_render::screenshot::{self, ReadbackBuffers};
use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests and bytes.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;
const FRAME_BYTES: u64 = (WIDTH * HEIGHT * 4) as u64;
const SHOTS: usize = 6;

#[test]
fn same_size_readbacks_reuse_the_gpu_buffer_and_the_pixel_vector() {
    let device = RenderDevice::headless()
        .unwrap_or_else(|err| panic!("the readback needs a GPU (lavapipe is one): {err}"));
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("capture source"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let pixels: Vec<u8> = (0..WIDTH * HEIGHT)
        .flat_map(|i| [(i % 251) as u8, (i % 241) as u8, (i % 239) as u8, 255])
        .collect();
    device.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(WIDTH * 4),
            rows_per_image: Some(HEIGHT),
        },
        wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
    );
    let mut buffers = ReadbackBuffers::new();
    let mut bytes = Vec::new();
    for shot in 0..SHOTS {
        let (image, cost) =
            alloc_counter::measure(|| screenshot::read_texture_rgba(&device, &texture, format, &mut buffers));
        let image = image.expect("readback");
        assert_eq!((image.width, image.height), (WIDTH, HEIGHT));
        assert!(
            image.rgba == pixels,
            "shot {shot}: the pixels read back as written"
        );
        bytes.push(cost.bytes);
        // The consumer (the agent's reply) gives the pixels back once encoded.
        screenshot::recycle(image);
    }
    eprintln!("frame {FRAME_BYTES} bytes; readback bytes per shot: {bytes:?}");
    assert!(
        bytes[0] >= FRAME_BYTES,
        "the counter sees the first capture's pixel vector: {bytes:?}"
    );
    for (shot, cost) in bytes.iter().enumerate().skip(1) {
        assert!(
            *cost < FRAME_BYTES / 16,
            "shot {shot}: a same-size readback asked for {cost} bytes (frame {FRAME_BYTES}): {bytes:?}"
        );
    }
    assert_eq!(
        buffers.created(),
        1,
        "one readback buffer for every same-size capture"
    );
    assert_eq!(buffers.len(), 1, "the window keeps it for the next capture");
    screenshot::release_screenshot_buffers();
    assert!(
        screenshot::FRAME_BUFFERS.is_empty(),
        "release frees the pooled pixels"
    );
}
