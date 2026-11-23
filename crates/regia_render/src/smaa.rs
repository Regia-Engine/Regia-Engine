// SPDX-License-Identifier: MPL-2.0
//! SMAA 1× (Jimenez et al., "SMAA: Enhanced Subpixel Morphological Antialiasing", 2012):
//! luma edges, blending weights from the area/search tables, neighbourhood blend.
//!
//! The post stack ([`crate::post_pass`]) runs it on the display-encoded image after the
//! tonemap when the frame's [`crate::FrameQuality::smaa`] is set: the non-temporal tiers'
//! AA (Mobile High, the Deck's [`crate::RenderProfile::non_temporal`] fallback). The shader
//! is `shaders/smaa.wgsl` (ported from Bevy); the tables are `luts/` (see its README).
//!
//! Three fullscreen passes share one bind-group layout. The edge and weight targets are
//! rebuilt only when the size changes; bind groups are cached by view.

use crate::frame_work::counted::*;
use std::borrow::Cow;
use std::collections::HashMap;

use crate::RenderError;

/// Area table: 160 × 560 `Rg8Unorm` texels, rows top to bottom.
const AREA_LUT: &[u8] = include_bytes!("../luts/smaa_area.rg8");
/// Search table: 64 × 16 `R8Unorm` texels, rows top to bottom.
const SEARCH_LUT: &[u8] = include_bytes!("../luts/smaa_search.r8");
const AREA_SIZE: (u32, u32) = (160, 560);
const SEARCH_SIZE: (u32, u32) = (64, 16);
/// Format of the edge target (two flags per pixel).
const EDGES_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rg8Unorm;
/// Format of the blending-weight target and of the stack's display image.
const WEIGHTS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

struct Targets {
    width: u32,
    height: u32,
    edges: wgpu::TextureView,
    weights: wgpu::TextureView,
}

struct Base {
    bgl: wgpu::BindGroupLayout,
    edge: wgpu::RenderPipeline,
    weight: wgpu::RenderPipeline,
    blend: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    area: wgpu::TextureView,
    search: wgpu::TextureView,
}

/// The persistent SMAA passes of one post chain.
#[derive(Default)]
pub(crate) struct SmaaPass {
    base: Option<Base>,
    targets: Option<Targets>,
    /// Groups keyed by (pass index, the three bound views).
    bg_cache: crate::bind_cache::BindCache<(u8, wgpu::TextureView, wgpu::TextureView, wgpu::TextureView)>,
}

impl SmaaPass {
    /// Builds the pipelines and uploads the tables (first call only) and sizes the targets.
    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        if self.base.is_none() {
            self.base = Some(build_base(device, belt, encoder)?);
        }
        if self
            .targets
            .as_ref()
            .is_none_or(|t| t.width != width || t.height != height)
        {
            let device = &crate::device::TrackedDevice::observe(device);
            self.targets = Some(Targets {
                width,
                height,
                edges: target(device, width, height, EDGES_FORMAT, "smaa edges"),
                weights: target(device, width, height, WEIGHTS_FORMAT, "smaa weights"),
            });
            self.bg_cache.clear();
        }
        Ok(())
    }

    /// Writes this frame's metrics uniform. Call after [`Self::prepare`], before the passes.
    pub(crate) fn write_uniform(
        &self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        let (Some(base), Some(targets)) = (self.base.as_ref(), self.targets.as_ref()) else {
            return;
        };
        let (w, h) = (targets.width as f32, targets.height as f32);
        let metrics = [1.0 / w, 1.0 / h, w, h];
        belt.write(device, encoder, &base.uniform, 0, bytemuck::bytes_of(&metrics));
    }

    /// Records edge detection, blending weights and the neighbourhood blend of `src` into
    /// `dest` (both [`Self::prepare`]'s size, `dest` `Rgba8Unorm`).
    pub(crate) fn record(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        src: &wgpu::TextureView,
        dest: &wgpu::TextureView,
    ) {
        let _span = tracing::info_span!("smaa").entered();
        let _crumb = crate::breadcrumbs::frame_pass("smaa");
        let (Some(base), Some(targets)) = (self.base.as_ref(), self.targets.as_ref()) else {
            return;
        };
        let device = &crate::device::TrackedDevice::observe(device);
        let passes: [(
            u8,
            &wgpu::RenderPipeline,
            &wgpu::TextureView,
            [&wgpu::TextureView; 3],
        ); 3] = [
            (0, &base.edge, &targets.edges, [src, &base.area, &base.search]),
            (
                1,
                &base.weight,
                &targets.weights,
                [&targets.edges, &base.area, &base.search],
            ),
            (2, &base.blend, dest, [src, &targets.weights, &base.search]),
        ];
        for (index, pipeline, out, [a, b, c]) in passes {
            let bg = self
                .bg_cache
                .get_or_insert_with((index, a.clone(), b.clone(), c.clone()), || {
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("smaa bg"),
                        layout: &base.bgl,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(a),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&base.sampler),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: base.uniform.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: wgpu::BindingResource::TextureView(b),
                            },
                            wgpu::BindGroupEntry {
                                binding: 4,
                                resource: wgpu::BindingResource::TextureView(c),
                            },
                        ],
                    })
                });
            let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(match index {
                    0 => "smaa edges",
                    1 => "smaa weights",
                    _ => "smaa blend",
                }),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: out,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                ..Default::default()
            });
            pass.counted_set_pipeline(pipeline);
            pass.counted_set_bind_group(0, &bg, &[]);
            pass.counted_draw(0..3, 0..1);
        }
    }
}

fn build_base(
    device: &wgpu::Device,
    belt: &mut crate::staging::StagingBelt,
    encoder: &mut wgpu::CommandEncoder,
) -> Result<Base, RenderError> {
    let device = &crate::device::TrackedDevice::observe(device);
    let module =
        crate::shader_lib::compose_source(include_str!("../shaders/smaa.wgsl"), "smaa.wgsl", HashMap::new())?;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("smaa"),
        source: wgpu::ShaderSource::Naga(Cow::Owned(module)),
    });
    let tex = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("smaa bgl"),
        entries: &[
            tex(0),
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            tex(3),
            tex(4),
        ],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("smaa layout"),
        bind_group_layouts: &[Some(&bgl)],
        immediate_size: 0,
    });
    let pipeline = |label: &str, vs: &str, fs: &str, format: wgpu::TextureFormat| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some(vs),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some(fs),
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
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        })
    };
    let edge = pipeline(
        "smaa edges",
        "edge_detection_vertex_main",
        "luma_edge_detection_fragment_main",
        EDGES_FORMAT,
    );
    let weight = pipeline(
        "smaa weights",
        "blending_weight_calculation_vertex_main",
        "blending_weight_calculation_fragment_main",
        WEIGHTS_FORMAT,
    );
    let blend = pipeline(
        "smaa blend",
        "neighborhood_blending_vertex_main",
        "neighborhood_blending_fragment_main",
        WEIGHTS_FORMAT,
    );
    // All SMAA samplers are linear and clamped (the searches rely on bilinear fetches).
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("smaa sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let uniform = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("smaa metrics"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let area = upload_lut(
        device,
        belt,
        encoder,
        AREA_LUT,
        AREA_SIZE,
        2,
        wgpu::TextureFormat::Rg8Unorm,
        "smaa area lut",
    )?;
    let search = upload_lut(
        device,
        belt,
        encoder,
        SEARCH_LUT,
        SEARCH_SIZE,
        1,
        wgpu::TextureFormat::R8Unorm,
        "smaa search lut",
    )?;
    Ok(Base {
        bgl,
        edge,
        weight,
        blend,
        sampler,
        uniform,
        area,
        search,
    })
}

/// Uploads one table through a padded staging copy (rows aligned to 256 bytes).
#[allow(clippy::too_many_arguments)]
fn upload_lut(
    device: &wgpu::Device,
    belt: &mut crate::staging::StagingBelt,
    encoder: &mut wgpu::CommandEncoder,
    bytes: &[u8],
    (width, height): (u32, u32),
    texel_bytes: u32,
    format: wgpu::TextureFormat,
    label: &str,
) -> Result<wgpu::TextureView, RenderError> {
    let row = (width * texel_bytes) as usize;
    if bytes.len() != row * height as usize {
        return Err(RenderError::Shader(format!(
            "{label}: {} bytes, expected {}",
            bytes.len(),
            row * height as usize
        )));
    }
    let aligned = row.div_ceil(256) * 256;
    let mut padded = vec![0u8; aligned * height as usize];
    for (y, src) in bytes.chunks_exact(row).enumerate() {
        padded[y * aligned..y * aligned + row].copy_from_slice(src);
    }
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("smaa lut upload"),
        size: padded.len() as u64,
        usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    belt.write(device, encoder, &staging, 0, &padded);
    encoder.copy_buffer_to_texture(
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(aligned as u32),
                rows_per_image: Some(height),
            },
        },
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    Ok(texture.create_view(&wgpu::TextureViewDescriptor::default()))
}

fn target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    label: &str,
) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_tables_have_smaa_sizes_and_values() {
        assert_eq!(AREA_LUT.len(), (AREA_SIZE.0 * AREA_SIZE.1 * 2) as usize);
        assert_eq!(SEARCH_LUT.len(), (SEARCH_SIZE.0 * SEARCH_SIZE.1) as usize);
        // SMAA's search texture encodes 0, 1 or 2 extra pixels as 0, 127 and 254.
        assert!(SEARCH_LUT.iter().all(|v| matches!(v, 0 | 127 | 254)));
        assert!(SEARCH_LUT.contains(&254) && SEARCH_LUT.contains(&127));
        // Neither half of the area table (orthogonal, diagonal) is blank.
        assert!(AREA_LUT.iter().any(|&v| v > 200));
        let (left, right) = AREA_LUT.split_at(AREA_LUT.len() / 2);
        assert!(left.iter().any(|&v| v != 0) && right.iter().any(|&v| v != 0));
    }
}
