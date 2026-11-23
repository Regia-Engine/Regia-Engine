// SPDX-License-Identifier: MPL-2.0
//! Cached RGBA16 to RG16 motion conversion; no filtering, unit or sign change (ADR-4656).

pub(crate) struct Motion {
    pipeline: wgpu::RenderPipeline,
    cached: Option<Cached>,
}
struct Cached {
    source: wgpu::TextureView,
    output: wgpu::TextureView,
    group: wgpu::BindGroup,
}
impl Motion {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("xess motion channels"),
            source: wgpu::ShaderSource::Wgsl(include_str!("motion.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("xess motion channels"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rg16Float,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        Self {
            pipeline,
            cached: None,
        }
    }

    pub(crate) fn record(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::TextureView,
    ) -> wgpu::TextureView {
        let _span = tracing::info_span!("xess_motion_channels").entered();
        let size = source.texture().size();
        let cached = self.cached.take().filter(|c| c.output.texture().size() == size);
        let output = cached.as_ref().map(|c| c.output.clone()).unwrap_or_else(|| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("xess RG16 motion"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rg16Float,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default())
        });
        let cached = cached.filter(|c| c.source == *source).unwrap_or_else(|| Cached {
            source: source.clone(),
            output: output.clone(),
            group: device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("xess motion channels"),
                layout: &self.pipeline.get_bind_group_layout(0),
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(source),
                }],
            }),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("xess motion channels"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &output,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &cached.group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.cached = Some(cached);
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_conversion_preserves_signed_xy_and_ignores_zw() {
        if std::env::var("REGIA_XESS_SMOKE").as_deref() != Ok("1") {
            return;
        }
        let path = std::path::PathBuf::from(std::env::var_os("REGIA_XESS_PATH").expect("runtime path"));
        let runtime = crate::Runtime::load(&path.join("libxess.dll")).expect("runtime");
        let (instance, _) = runtime
            .create_instance(&wgpu::InstanceDescriptor {
                backends: wgpu::Backends::VULKAN,
                ..wgpu::InstanceDescriptor::new_without_display_handle()
            })
            .expect("instance");
        let requested = std::env::var("REGIA_XESS_GPU").ok();
        let adapter = regia_core::tasks::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN))
            .into_iter()
            .find(|adapter| {
                let info = adapter.get_info();
                info.device_type != wgpu::DeviceType::Cpu
                    && requested
                        .as_ref()
                        .is_none_or(|name| info.name.contains(name.as_str()))
            })
            .expect("hardware adapter");
        let (device, queue, _) = runtime
            .request_device(&adapter, &wgpu::DeviceDescriptor::default())
            .expect("device");
        let source = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("motion channel regression"),
            size: wgpu::Extent3d {
                width: 3,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let pixels: [[u16; 4]; 6] = [
            [0x3c00, 0xbc00, 0x4000, 0x4400],
            [0x3800, 0xb400, 0x4800, 0x4c00],
            [0, 0, 0x5000, 0x5400],
            [0xc000, 0x4000, 0x5800, 0x5c00],
            [0x3000, 0xb000, 0x6000, 0x6400],
            [0xb800, 0x3400, 0x6800, 0x6c00],
        ];
        let bytes: Vec<_> = pixels.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
        queue.write_texture(
            source.as_image_copy(),
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(24),
                rows_per_image: Some(2),
            },
            source.size(),
        );
        let mut converter = Motion::new(&device);
        let mut encoder = device.create_command_encoder(&Default::default());
        let output = converter.record(&device, &mut encoder, &source.create_view(&Default::default()));
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("motion channel readback"),
            size: 512,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            output.texture().as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(2),
                },
            },
            source.size(),
        );
        queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("completion");
        rx.recv().expect("callback").expect("map");
        let mapped = readback.slice(..).get_mapped_range().expect("mapped range");
        for (index, pixel) in pixels.iter().enumerate() {
            let offset = (index / 3) * 256 + (index % 3) * 4;
            assert_eq!(&mapped[offset..offset + 2], &pixel[0].to_le_bytes());
            assert_eq!(&mapped[offset + 2..offset + 4], &pixel[1].to_le_bytes());
        }
    }
}
