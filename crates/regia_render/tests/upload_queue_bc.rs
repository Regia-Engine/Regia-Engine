// SPDX-License-Identifier: MPL-2.0
//! `UploadQueue` writes a whole BC7 mip chain, down to the 2×2 and 1×1 levels (smaller than one
//! 4×4 block), with no validation error, under a budget small enough to band the big levels.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use regia_render::{RenderDevice, UploadBytes, UploadQueue};

const SIZE: u32 = 256;
const MIPS: u32 = 9;

fn level_bytes(mip: u32) -> usize {
    let blocks = (SIZE >> mip).max(1).div_ceil(4) as usize;
    blocks * blocks * 16
}

#[test]
fn bc7_chain_uploads_every_level() {
    let base = RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
    let bc = wgpu::Features::TEXTURE_COMPRESSION_BC;
    assert!(
        base.adapter.features().contains(bc),
        "adapter has no BC compression"
    );
    let (device, queue) = regia_core::tasks::block_on(base.adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("upload_queue_bc"),
        required_limits: base.device.limits(),
        required_features: base.device.features() | bc,
        ..Default::default()
    }))
    .expect("BC device");
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("bc7 chain"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: MIPS,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bc7RgbaUnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let mut uploads = UploadQueue::default();
    let done = Arc::new(AtomicU32::new(0));
    for mip in 0..MIPS {
        let done = done.clone();
        uploads.push_texture_level(
            format!("mip{mip}"),
            &texture,
            mip,
            0,
            UploadBytes::from(vec![0u8; level_bytes(mip)]),
            Some(Box::new(move || {
                done.fetch_add(1, Ordering::Relaxed);
            })),
        );
    }
    let mut pumps = 0;
    while !uploads.is_idle() {
        uploads.pump(&queue);
        pumps += 1;
        assert!(pumps < 10_000, "upload queue never drained");
    }
    queue.submit([]);
    let error = regia_core::tasks::block_on(scope.pop());
    assert!(error.is_none(), "validation error: {error:?}");
    assert_eq!(done.load(Ordering::Relaxed), MIPS, "every level's callback ran");
}
