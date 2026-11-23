// SPDX-License-Identifier: MPL-2.0
//! Held-view prewarm tests for `post_pass` (ADR-1361), in their own file: the frame module's
//! `frame_path_has_no_raw_queue_submit` scans `post_pass.rs` for submit calls, and these GPU
//! tests submit their own encoders.

use super::*;

const SIZE: u32 = 64;

fn view(device: &wgpu::Device, label: &str, format: wgpu::TextureFormat) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

/// Records and submits one post frame reading `hdr`; returns the bind-group creates it made.
fn frame(
    render: &crate::RenderDevice,
    post: &mut PostChain,
    belt: &mut crate::staging::StagingBelt,
    hdr: &wgpu::TextureView,
    out: &wgpu::TextureView,
) -> u64 {
    let before = crate::frame_stats::bind_group_creates();
    belt.begin_frame();
    let mut encoder = render
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("post test"),
        });
    post.record(
        &render.device,
        belt,
        &mut encoder,
        hdr,
        SIZE,
        SIZE,
        &PostSettings::default(),
        out,
        wgpu::TextureFormat::Rgba8Unorm,
        [0.0; 4],
        None,
        PostAa::default(),
    )
    .expect("post record");
    belt.flush(&mut encoder);
    // Like the frame: the chunks the copies read are unmapped before the submit.
    belt.upload(&render.tracked_queue());
    render.queue.submit([encoder.finish()]);
    belt.after_submit();
    post.map_after_submit();
    let _ = render.device.poll(wgpu::PollType::wait_indefinitely());
    crate::frame_stats::bind_group_creates() - before
}

/// ADR-1361: the first held-view frame hands post an HDR view it has not seen. Prewarming
/// that view builds its post and histogram groups, so the swap creates none; the same swap
/// to a view that was not prewarmed does create some (the control).
#[test]
fn prewarmed_input_swap_creates_no_bind_groups() {
    let render = crate::test_device();
    let device = &render.device;
    let out = view(device, "post test out", wgpu::TextureFormat::Rgba8Unorm);
    let a = view(device, "hdr a", wgpu::TextureFormat::Rgba16Float);
    let warmed = view(device, "hdr warmed", wgpu::TextureFormat::Rgba16Float);
    let cold = view(device, "hdr cold", wgpu::TextureFormat::Rgba16Float);
    let mut post = PostChain::default();
    let mut belt = crate::staging::StagingBelt::default();
    for _ in 0..4 {
        frame(&render, &mut post, &mut belt, &a, &out);
    }
    let created = post.prewarm_input(device, &warmed);
    assert!(created > 0, "prewarm built no groups for a new input");
    let swapped = frame(&render, &mut post, &mut belt, &warmed, &out);
    assert_eq!(swapped, 0, "a prewarmed input created {swapped} bind groups");
    let control = frame(&render, &mut post, &mut belt, &cold, &out);
    assert!(control > 0, "the control swap to an unseen view created nothing");
}
