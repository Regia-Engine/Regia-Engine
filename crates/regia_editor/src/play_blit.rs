// SPDX-License-Identifier: MPL-2.0
//! The play window's letterbox blit (ADR-7283): the play world's picture, rendered at the chosen
//! play resolution, is scaled into its [`Letterbox`] rect of the window's surface and the bars
//! around it are cleared to black. One fullscreen triangle drawn with the render pass viewport set
//! to the rect, sampling the picture with a linear filter.
//!
//! The picture holds sRGB-encoded bytes in a non-sRGB texture, so the target view must be
//! non-sRGB too (the bytes pass through unchanged, as `wgpu::util::TextureBlitter` did before).

use regia_render::RenderDevice;

use crate::play_window::Letterbox;

const SHADER: &str = r"
@group(0) @binding(0) var picture: texture_2d<f32>;
@group(0) @binding(1) var picture_sampler: sampler;

struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VOut {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return VOut(vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0), uv);
}

@fragment fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    return textureSample(picture, picture_sampler, in.uv);
}
";

/// The blit pipeline for one target format (make it once per play window).
#[derive(Debug)]
pub struct LetterboxBlit {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl LetterboxBlit {
    /// A blit that writes `format` targets.
    #[must_use]
    pub fn new(device: &RenderDevice, format: wgpu::TextureFormat) -> Self {
        let gpu = &device.device;
        let module = gpu.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("play window letterbox"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(SHADER)),
        });
        let layout = gpu.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("play window letterbox"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = gpu.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("play window letterbox"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = gpu.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("play window letterbox"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = gpu.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("play window letterbox"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            pipeline,
            layout,
            sampler,
        }
    }

    /// Clears `target` to black and draws `picture` scaled into `rect` of it.
    pub fn draw(
        &self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        picture: &wgpu::TextureView,
        target: &wgpu::TextureView,
        rect: Letterbox,
    ) {
        let bind_group = device.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("play window letterbox"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(picture),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("play window letterbox"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
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
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        pass.set_viewport(
            rect.x as f32,
            rect.y as f32,
            rect.width as f32,
            rect.height as f32,
            0.0,
            1.0,
        );
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On the GPU (lavapipe in CI): a 18 x 32 red picture fitted into a 64 x 36 target lands in the
    /// middle 20 columns (x 22..42); the bars are black.
    #[test]
    fn the_picture_lands_in_its_letterbox_rect_and_the_bars_are_black() {
        let device = match RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
        };
        let gpu = &device.device;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let picture = gpu.create_texture(&wgpu::TextureDescriptor {
            label: Some("picture"),
            size: wgpu::Extent3d {
                width: 18,
                height: 32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let red: Vec<u8> = [200u8, 10, 10, 255].repeat(18 * 32);
        device.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &picture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &red,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(18 * 4),
                rows_per_image: Some(32),
            },
            wgpu::Extent3d {
                width: 18,
                height: 32,
                depth_or_array_layers: 1,
            },
        );
        let (tw, th) = (64u32, 36u32);
        let target = gpu.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: wgpu::Extent3d {
                width: tw,
                height: th,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let rect = Letterbox::fit([18, 32], [tw, th]);
        assert_eq!(rect.to_array(), [22, 0, 20, 36]);
        let blit = LetterboxBlit::new(&device, format);
        let readback = gpu.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(tw * 4 * th),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        blit.draw(
            &device,
            &mut encoder,
            &picture.create_view(&wgpu::TextureViewDescriptor::default()),
            &target.create_view(&wgpu::TextureViewDescriptor::default()),
            rect,
        );
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    // 64 * 4 = 256: already the copy row alignment.
                    bytes_per_row: Some(tw * 4),
                    rows_per_image: Some(th),
                },
            },
            wgpu::Extent3d {
                width: tw,
                height: th,
                depth_or_array_layers: 1,
            },
        );
        device.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        gpu.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        rx.recv().expect("callback").expect("mapped");
        let bytes = readback
            .slice(..)
            .get_mapped_range()
            .expect("mapped range")
            .to_vec();
        let pixel = |x: u32, y: u32| {
            let i = ((y * tw + x) * 4) as usize;
            [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
        };
        for y in [0, 17, 35] {
            assert_eq!(pixel(0, y), [0, 0, 0, 255], "left bar at y {y}");
            assert_eq!(pixel(21, y), [0, 0, 0, 255], "last bar column at y {y}");
            assert_eq!(pixel(22, y), [200, 10, 10, 255], "first picture column at y {y}");
            assert_eq!(pixel(41, y), [200, 10, 10, 255], "last picture column at y {y}");
            assert_eq!(pixel(42, y), [0, 0, 0, 255], "right bar at y {y}");
            assert_eq!(pixel(63, y), [0, 0, 0, 255], "right edge at y {y}");
        }
    }
}
