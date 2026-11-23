// SPDX-License-Identifier: MPL-2.0
//! The desktop mirror (spec 58 § 2.6): the window shows the left eye, the right eye, or both side
//! by side, each cropped to fill so no bars appear and nothing stretches. Android has no mirror.
//!
//! A [`regia_render::OverlayDraw`] layer: the window's frame draws it under the runtime UI, so the
//! mirror needs no surface of its own and shows up in `capture.screenshot` too.

use std::sync::{Arc, Mutex};

use regia_render::{OverlayDraw, RenderDevice};

use crate::config::Mirror;

/// One rectangle of the mirror.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MirrorRect {
    /// Which eye it shows.
    pub eye: usize,
    /// Source rectangle in the eye image: u, v, width, height (0 to 1).
    pub source: [f32; 4],
    /// Destination in the window: x, y, width, height, pixels.
    pub target: [f32; 4],
}

/// The largest centered rectangle of aspect `target_aspect` inside a unit image of aspect
/// `image_aspect`.
fn crop_to_fill(image_aspect: f32, target_aspect: f32) -> [f32; 4] {
    if target_aspect >= image_aspect {
        let height = image_aspect / target_aspect;
        [0.0, (1.0 - height) * 0.5, 1.0, height]
    } else {
        let width = target_aspect / image_aspect;
        [(1.0 - width) * 0.5, 0.0, width, 1.0]
    }
}

/// The rectangles `mode` draws into a `window`-sized target from `eye`-sized images. Empty when
/// the mode is off or a size is zero.
pub fn mirror_rects(mode: Mirror, eye: [u32; 2], window: [u32; 2]) -> Vec<MirrorRect> {
    if eye.contains(&0) || window.contains(&0) {
        return Vec::new();
    }
    let image_aspect = eye[0] as f32 / eye[1] as f32;
    let (w, h) = (window[0] as f32, window[1] as f32);
    let one = |index: usize| MirrorRect {
        eye: index,
        source: crop_to_fill(image_aspect, w / h),
        target: [0.0, 0.0, w, h],
    };
    match mode {
        Mirror::Off => Vec::new(),
        Mirror::Left => vec![one(0)],
        Mirror::Right => vec![one(1)],
        Mirror::Both => {
            let half = (w * 0.5).floor();
            let source = crop_to_fill(image_aspect, half / h);
            vec![
                MirrorRect {
                    eye: 0,
                    source,
                    target: [0.0, 0.0, half, h],
                },
                MirrorRect {
                    eye: 1,
                    source,
                    target: [half, 0.0, w - half, h],
                },
            ]
        }
    }
}

/// What the mirror shows: both eyes' images and the mode.
#[derive(Clone, Default)]
pub struct MirrorSource {
    /// The eye images (the composed display images on the simulator).
    pub views: Option<[wgpu::TextureView; 2]>,
    /// Their size.
    pub extent: [u32; 2],
    /// The mode.
    pub mode: Mirror,
}

/// The source, shared between the frame system and the overlay layer.
#[derive(Clone, Default)]
pub struct SharedMirror(Arc<Mutex<MirrorSource>>);

impl SharedMirror {
    /// Replaces the source.
    pub fn set(&self, source: MirrorSource) {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = source;
    }

    fn get(&self) -> MirrorSource {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Order of the mirror's overlay layer: under the runtime UI.
pub const MIRROR_ORDER: i32 = regia_render::WindowOverlay::UI_ORDER - 100;

/// Name of the mirror's overlay layer.
pub const MIRROR_LAYER: &str = "xr_mirror";

const SLOT: u64 = 256;
const UNIFORM_SIZE: u64 = 32;

struct Gpu {
    module: wgpu::ShaderModule,
    bgl: wgpu::BindGroupLayout,
    layout: wgpu::PipelineLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
}

/// Draws the mirror.
#[derive(Default)]
pub struct MirrorPainter {
    source: SharedMirror,
    gpu: Option<Gpu>,
    pipelines: std::collections::HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
}

impl MirrorPainter {
    /// A painter showing `source`.
    pub fn new(source: SharedMirror) -> Self {
        MirrorPainter {
            source,
            ..MirrorPainter::default()
        }
    }

    fn ensure(&mut self, d: &wgpu::Device, format: wgpu::TextureFormat) {
        let gpu = self.gpu.get_or_insert_with(|| {
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("xr mirror"),
                source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/xr_mirror.wgsl").into()),
            });
            let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("xr mirror"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: true,
                            min_binding_size: wgpu::BufferSize::new(UNIFORM_SIZE),
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
            let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("xr mirror"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("xr mirror"),
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            let uniforms = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("xr mirror"),
                size: SLOT * 2,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            Gpu {
                module,
                bgl,
                layout,
                sampler,
                uniforms,
            }
        });
        let gpu = &*gpu;
        self.pipelines.entry(format).or_insert_with(|| {
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("xr mirror"),
                layout: Some(&gpu.layout),
                vertex: wgpu::VertexState {
                    module: &gpu.module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &gpu.module,
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
            })
        });
    }
}

/// Whether writing to `format` needs the shader's sRGB encode: a plain 8-bit target stores the
/// encoded bytes itself; an sRGB target encodes on write, a float target stays linear.
fn needs_encode(format: wgpu::TextureFormat) -> bool {
    matches!(
        format,
        wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Bgra8Unorm
    )
}

impl OverlayDraw for MirrorPainter {
    fn draw(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) {
        let _span = tracing::trace_span!("xr_mirror").entered();
        let source = self.source.get();
        let rects = mirror_rects(source.mode, source.extent, [width, height]);
        let Some(views) = source.views.as_ref().filter(|_| !rects.is_empty()) else {
            return;
        };
        let d = &device.device;
        self.ensure(d, format);
        let (Some(gpu), Some(pipeline)) = (self.gpu.as_ref(), self.pipelines.get(&format)) else {
            return;
        };
        let encode = if needs_encode(format) { 1.0_f32 } else { 0.0 };
        let groups: Vec<wgpu::BindGroup> = rects
            .iter()
            .enumerate()
            .map(|(slot, rect)| {
                let [u, v, w, h] = rect.source;
                let values = [u, v, w, h, encode, 0.0, 0.0, 0.0];
                device
                    .queue
                    .write_buffer(&gpu.uniforms, slot as u64 * SLOT, bytemuck::cast_slice(&values));
                d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("xr mirror"),
                    layout: &gpu.bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &gpu.uniforms,
                                offset: 0,
                                size: wgpu::BufferSize::new(UNIFORM_SIZE),
                            }),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&views[rect.eye]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                        },
                    ],
                })
            })
            .collect();
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("xr mirror"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(pipeline);
        for (slot, (rect, group)) in rects.iter().zip(&groups).enumerate() {
            let [x, y, w, h] = rect.target;
            pass.set_viewport(x, y, w, h, 0.0, 1.0);
            pass.set_bind_group(0, group, &[(slot as u64 * SLOT) as u32]);
            pass.draw(0..3, 0..1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_and_empty_sizes_draw_nothing() {
        assert!(mirror_rects(Mirror::Off, [100, 100], [100, 100]).is_empty());
        assert!(mirror_rects(Mirror::Left, [0, 100], [100, 100]).is_empty());
        assert!(mirror_rects(Mirror::Left, [100, 100], [100, 0]).is_empty());
    }

    #[test]
    fn a_wide_window_crops_the_tall_eye_vertically_and_keeps_its_shape() {
        let rects = mirror_rects(Mirror::Right, [1000, 1000], [1600, 800]);
        assert_eq!(rects.len(), 1);
        let r = rects[0];
        assert_eq!(r.eye, 1);
        assert_eq!(r.source, [0.0, 0.25, 1.0, 0.5]);
        assert_eq!(r.target, [0.0, 0.0, 1600.0, 800.0]);
        let shown = (r.source[2] * 1000.0) / (r.source[3] * 1000.0);
        assert!((shown - 2.0).abs() < 1e-5, "no stretch");
    }

    #[test]
    fn a_tall_window_crops_horizontally() {
        let r = mirror_rects(Mirror::Left, [1600, 800], [800, 800])[0];
        assert_eq!(r.source, [0.25, 0.0, 0.5, 1.0]);
    }

    #[test]
    fn both_eyes_split_the_window_and_cover_every_pixel() {
        let rects = mirror_rects(Mirror::Both, [1000, 1000], [1601, 800]);
        assert_eq!(rects.len(), 2);
        assert_eq!((rects[0].eye, rects[1].eye), (0, 1));
        assert_eq!(rects[0].target, [0.0, 0.0, 800.0, 800.0]);
        assert_eq!(rects[1].target, [800.0, 0.0, 801.0, 800.0]);
        assert_eq!(rects[0].source, [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn only_plain_eight_bit_targets_take_the_encode() {
        assert!(needs_encode(wgpu::TextureFormat::Bgra8Unorm));
        assert!(!needs_encode(wgpu::TextureFormat::Bgra8UnormSrgb));
        assert!(!needs_encode(wgpu::TextureFormat::Rgba16Float));
    }
}
