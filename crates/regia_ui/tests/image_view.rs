// SPDX-License-Identifier: MPL-2.0
//! A GPU texture shown as a UI image without a copy (v0.1.6): a render-to-texture camera's
//! image on a HUD panel ([`regia_ui::UiRenderer::add_image_view`]).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_render::RenderDevice;
use regia_ui::{OffscreenTarget, TextureRef, UiColor, UiQuad, UiRenderer};

#[test]
fn an_external_texture_view_draws_as_an_image() {
    let device = match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    // Stands in for a texture view's target: an sRGB texture written on the GPU side.
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("external view"),
        size: wgpu::Extent3d {
            width: 32,
            height: 16,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let red: Vec<u8> = (0..32 * 16).flat_map(|_| [230u8, 20, 10, 255]).collect();
    device.queue.write_texture(
        texture.as_image_copy(),
        &red,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(32 * 4),
            rows_per_image: Some(16),
        },
        texture.size(),
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut renderer = UiRenderer::default();
    renderer.add_image_view(&device, 77, &view);
    assert_eq!(renderer.image_size(77), Some([32, 16]));

    let target = OffscreenTarget::new(&device, 64, 64);
    let grey: Vec<u8> = (0..64 * 64).flat_map(|_| [40u8, 40, 40, 255]).collect();
    assert!(target.upload(&device, &grey));
    let mut panel = UiQuad::fill([8.0, 8.0], [48.0, 24.0], UiColor::WHITE);
    panel.texture = TextureRef::Image(77);
    renderer.set_quads(vec![panel]);
    target.draw_ui(&device, &mut renderer);
    let out = target.read(&device).expect("readback");
    let at = |x: u32, y: u32| {
        let i = ((y * 64 + x) * 4) as usize;
        [out[i], out[i + 1], out[i + 2]]
    };
    let inside = at(32, 20);
    assert!(
        inside[0] > 200 && inside[1] < 60 && inside[2] < 60,
        "the panel does not show the texture: {inside:?}"
    );
    assert_eq!(at(2, 2), [40, 40, 40], "outside the panel");
}
