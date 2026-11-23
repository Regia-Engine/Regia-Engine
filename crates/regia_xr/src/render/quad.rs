// SPDX-License-Identifier: MPL-2.0
//! Spatial panels as quad layers (spec 58 § 2.10): what a game declares ([`XrPanels`]), how a panel
//! is placed in tracking space, and the painter that draws one over an eye image. A runtime draws
//! the composed panels itself; the painter is the simulator's compositor and the fallback for the
//! panels over the runtime's layer budget.

use regia_app::prelude::*;
use regia_core::glam::{Mat4, Vec2, Vec3};
use regia_platform::xr::{LayerSpace, Pose, QuadLayer, SubImage, order_quads};

/// Most panels a frame draws.
pub const MAX_PANELS: usize = 32;

/// What a panel shows.
#[derive(Clone, Debug)]
pub enum PanelImage {
    /// One sRGB colour, straight alpha.
    Solid([u8; 4]),
    /// A texture a panel renders into (a `regia_ui` world-space panel's target).
    Texture {
        /// The image, sampled in linear.
        view: wgpu::TextureView,
        /// Its size in pixels.
        size: [u32; 2],
    },
}

/// A flat panel over the scene.
#[derive(Clone, Debug)]
pub struct XrPanel {
    /// Stable id (`xr.set_quad`).
    pub id: u32,
    /// Higher draws on top; equal priorities draw in id order.
    pub priority: i32,
    /// What it is locked to.
    pub space: LayerSpace,
    /// Its center and orientation in `space`; the panel faces +Z of this pose.
    pub pose: Pose,
    /// Width and height, meters.
    pub size_m: [f32; 2],
    /// Its image.
    pub image: PanelImage,
}

impl XrPanel {
    /// The layer record submitted for this panel.
    pub fn layer(&self) -> QuadLayer {
        let rect = match &self.image {
            PanelImage::Solid(_) => [0, 0, 1, 1],
            PanelImage::Texture { size, .. } => [0, 0, size[0], size[1]],
        };
        QuadLayer {
            id: self.id,
            priority: self.priority,
            space: self.space,
            pose: self.pose,
            size_m: self.size_m,
            image: SubImage { array_index: 0, rect },
        }
    }
}

/// The panels of the session.
#[derive(Resource, Clone, Debug, Default)]
pub struct XrPanels {
    panels: Vec<XrPanel>,
    by_id: std::collections::HashMap<u32, usize>,
}

impl XrPanels {
    /// Adds `panel` or replaces the panel with its id. `false` when [`MAX_PANELS`] are in use.
    pub fn set(&mut self, panel: XrPanel) -> bool {
        if let Some(&index) = self.by_id.get(&panel.id) {
            self.panels[index] = panel;
            return true;
        }
        if self.panels.len() >= MAX_PANELS {
            return false;
        }
        self.by_id.insert(panel.id, self.panels.len());
        self.panels.push(panel);
        true
    }

    /// Removes panel `id`; true when it existed.
    pub fn remove(&mut self, id: u32) -> bool {
        let Some(index) = self.by_id.get(&id).copied() else {
            return false;
        };
        self.panels.remove(index);
        self.by_id.clear();
        for (index, panel) in self.panels.iter().enumerate() {
            self.by_id.insert(panel.id, index);
        }
        true
    }

    /// The panels.
    pub fn iter(&self) -> impl Iterator<Item = &XrPanel> {
        self.panels.iter()
    }

    /// How many there are.
    pub fn len(&self) -> usize {
        self.panels.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.panels.is_empty()
    }

    /// The layer records in draw order.
    pub fn ordered_layers(&self) -> Vec<QuadLayer> {
        let mut layers: Vec<QuadLayer> = self.panels.iter().map(XrPanel::layer).collect();
        order_quads(&mut layers);
        layers
    }

    /// The panel with `id`.
    pub fn get(&self, id: u32) -> Option<&XrPanel> {
        self.by_id.get(&id).map(|&index| &self.panels[index])
    }
}

/// A panel's pose in tracking space. `head` is the head in tracking space, `world` is world from
/// tracking space.
pub fn panel_in_tracking(layer: &QuadLayer, head: &Pose, world: &Pose) -> Pose {
    match layer.space {
        LayerSpace::Head => head.compose(&layer.pose),
        LayerSpace::Rig => layer.pose,
        LayerSpace::World => world.inverse().compose(&layer.pose),
    }
}

const SLOT: u64 = 256;
const PANEL_UNIFORM: u64 = 96;

/// Draws panels over an eye image.
#[derive(Default)]
pub struct QuadPainter {
    gpu: Option<Gpu>,
    pipelines: std::collections::HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    /// One bind group per panel slot, reused while that slot shows the same view.
    slots: Vec<SlotGroup>,
    /// Packed panel uniforms for one `draw` call.
    scratch: Vec<u8>,
}

struct SlotGroup {
    view: wgpu::TextureView,
    group: wgpu::BindGroup,
}

struct Gpu {
    module: wgpu::ShaderModule,
    bgl: wgpu::BindGroupLayout,
    layout: wgpu::PipelineLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    white: wgpu::TextureView,
}

/// One panel to draw into one eye.
pub struct PanelDraw<'a> {
    /// Clip from tracking space for the eye.
    pub clip_from_tracking: Mat4,
    /// The panel's pose in tracking space.
    pub pose: Pose,
    /// The record.
    pub layer: &'a QuadLayer,
    /// The image.
    pub image: &'a PanelImage,
}

fn srgb_to_linear(byte: u8) -> f32 {
    let c = f32::from(byte) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

impl QuadPainter {
    fn ensure(&mut self, d: &wgpu::Device, q: &wgpu::Queue, format: wgpu::TextureFormat) {
        let gpu = self.gpu.get_or_insert_with(|| {
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("xr quad"),
                source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/xr_quad.wgsl").into()),
            });
            let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("xr quad"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: true,
                            min_binding_size: wgpu::BufferSize::new(PANEL_UNIFORM),
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
                label: Some("xr quad"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("xr quad"),
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            let uniforms = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("xr quads"),
                size: SLOT * MAX_PANELS as u64 * 2,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let white = d.create_texture(&wgpu::TextureDescriptor {
                label: Some("xr quad white"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            q.write_texture(
                white.as_image_copy(),
                &[255; 4],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
            let white = white.create_view(&wgpu::TextureViewDescriptor::default());
            Gpu {
                module,
                bgl,
                layout,
                sampler,
                uniforms,
                white,
            }
        });
        let gpu = &*gpu;
        self.pipelines.entry(format).or_insert_with(|| {
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("xr quad"),
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
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::COLOR,
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

    /// Records `panels`, in order, over `color`, using uniform slots from 0.
    pub fn draw(
        &mut self,
        d: &wgpu::Device,
        q: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: (&wgpu::TextureView, wgpu::TextureFormat),
        panels: &[PanelDraw<'_>],
    ) {
        self.draw_from(d, q, encoder, target, 0, panels);
    }

    /// Records `panels` over `color`, writing uniforms at `slot_base`.
    ///
    /// Two eyes of one submit pass disjoint bases, so each eye's matrices survive
    /// until that submit. The bind group for a slot is kept while the slot shows
    /// the same texture view.
    pub(crate) fn draw_from(
        &mut self,
        d: &wgpu::Device,
        q: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        (color, format): (&wgpu::TextureView, wgpu::TextureFormat),
        slot_base: u32,
        panels: &[PanelDraw<'_>],
    ) {
        if panels.is_empty() {
            return;
        }
        self.ensure(d, q, format);
        let Some(pipeline) = self.pipelines.get(&format).cloned() else {
            return;
        };
        let (bgl, uniforms, sampler, white) = {
            let Some(gpu) = self.gpu.as_ref() else {
                return;
            };
            (
                gpu.bgl.clone(),
                gpu.uniforms.clone(),
                gpu.sampler.clone(),
                gpu.white.clone(),
            )
        };
        let count = panels.len().min(MAX_PANELS);
        let stride = SLOT as usize;
        self.scratch.resize(count * stride, 0);
        self.scratch.fill(0);
        for (index, panel) in panels.iter().take(count).enumerate() {
            write_panel_uniform(&mut self.scratch[index * stride..], panel);
            let view = match panel.image {
                PanelImage::Solid(_) => &white,
                PanelImage::Texture { view, .. } => view,
            };
            if self.slots.get(index).is_some_and(|slot| &slot.view == view) {
                continue;
            }
            crate::perf::quad_bind_group();
            let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("xr quad"),
                layout: &bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &uniforms,
                            offset: 0,
                            size: wgpu::BufferSize::new(PANEL_UNIFORM),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            });
            let stored = SlotGroup {
                view: view.clone(),
                group,
            };
            if index == self.slots.len() {
                self.slots.push(stored);
            } else {
                self.slots[index] = stored;
            }
        }
        q.write_buffer(
            &uniforms,
            u64::from(slot_base) * SLOT,
            &self.scratch[..count * stride],
        );
        crate::perf::quad_uniform_write();
        let groups: Vec<(u32, wgpu::BindGroup)> = self
            .slots
            .iter()
            .take(count)
            .enumerate()
            .map(|(index, slot)| {
                let offset = (u64::from(slot_base) + index as u64) * SLOT;
                (offset as u32, slot.group.clone())
            })
            .collect();
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("xr quads"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&pipeline);
        for (offset, group) in &groups {
            pass.set_bind_group(0, group, &[*offset]);
            pass.draw(0..6, 0..1);
        }
    }
}

fn write_panel_uniform(dst: &mut [u8], panel: &PanelDraw<'_>) {
    let matrix = panel.clip_from_tracking * panel.pose.to_matrix();
    let half = Vec2::new(panel.layer.size_m[0], panel.layer.size_m[1]) * 0.5;
    let tint = match panel.image {
        PanelImage::Solid([r, g, b, a]) => [
            srgb_to_linear(*r),
            srgb_to_linear(*g),
            srgb_to_linear(*b),
            f32::from(*a) / 255.0,
        ],
        PanelImage::Texture { .. } => [1.0; 4],
    };
    let mut at = 0usize;
    copy_at(dst, &mut at, bytemuck::cast_slice(&matrix.to_cols_array()));
    copy_at(dst, &mut at, bytemuck::cast_slice(&[half.x, half.y, 0.0, 0.0]));
    copy_at(dst, &mut at, bytemuck::cast_slice(&tint));
}

fn copy_at(dst: &mut [u8], at: &mut usize, bytes: &[u8]) {
    let end = *at + bytes.len();
    dst[*at..end].copy_from_slice(bytes);
    *at = end;
}

/// Where a point of panel space lands in clip space: what `vs_main` computes, for tests.
pub fn panel_point_clip(clip_from_tracking: Mat4, pose: &Pose, local: Vec2) -> regia_core::glam::Vec4 {
    clip_from_tracking * pose.to_matrix() * Vec3::new(local.x, local.y, 0.0).extend(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::glam::Quat;

    /// Audit v034 WGSL M2: no XR shader puts a struct or array in a function-local `var`
    /// (ADR-026), which Mesa's SPIR-V validation refuses; these are outside `cargo xtask
    /// check-shaders`.
    #[test]
    fn xr_shaders_have_no_function_layout_errors() {
        for (name, source) in [
            (
                "xr_eye_composite.wgsl",
                include_str!("../../shaders/xr_eye_composite.wgsl"),
            ),
            ("xr_mirror.wgsl", include_str!("../../shaders/xr_mirror.wgsl")),
            ("xr_quad.wgsl", include_str!("../../shaders/xr_quad.wgsl")),
            (
                "xr_stereo_forward.wgsl",
                include_str!("../../shaders/xr_stereo_forward.wgsl"),
            ),
        ] {
            let found = regia_render::wgsl_function_layout_errors(name, source).expect("emits");
            assert!(found.is_empty(), "{name}: {found:?}");
        }
    }

    fn panel(id: u32, priority: i32) -> XrPanel {
        XrPanel {
            id,
            priority,
            space: LayerSpace::Rig,
            pose: Pose::IDENTITY,
            size_m: [1.0, 1.0],
            image: PanelImage::Solid([255, 0, 0, 255]),
        }
    }

    #[test]
    fn panels_replace_by_id_cap_at_the_limit_and_order_for_drawing() {
        let mut panels = XrPanels::default();
        assert!(panels.set(panel(2, 5)));
        assert!(panels.set(panel(1, 5)));
        assert!(panels.set(panel(3, -1)));
        assert!(panels.set(panel(2, 9)), "same id replaces");
        assert_eq!(panels.len(), 3);
        let order: Vec<u32> = panels.ordered_layers().iter().map(|l| l.id).collect();
        assert_eq!(order, [3, 1, 2]);
        assert!(panels.remove(3));
        assert!(!panels.remove(3));
        for id in 10..10 + MAX_PANELS as u32 {
            panels.set(panel(id, 0));
        }
        assert_eq!(panels.len(), MAX_PANELS);
        assert!(!panels.set(panel(99, 0)), "full");
    }

    #[test]
    fn each_lock_follows_its_own_frame() {
        let head = Pose {
            position: Vec3::new(0.0, 1.7, 0.0),
            orientation: Quat::from_rotation_y(0.5),
        };
        let world = Pose {
            position: Vec3::new(5.0, 0.0, 0.0),
            orientation: Quat::IDENTITY,
        };
        let layer = |space| QuadLayer {
            id: 1,
            priority: 0,
            space,
            pose: Pose {
                position: Vec3::new(0.0, 0.0, -1.0),
                orientation: Quat::IDENTITY,
            },
            size_m: [1.0, 1.0],
            image: SubImage {
                array_index: 0,
                rect: [0, 0, 1, 1],
            },
        };
        let in_head = panel_in_tracking(&layer(LayerSpace::Head), &head, &world);
        let expected = head.position + head.orientation * Vec3::NEG_Z;
        assert!((in_head.position - expected).length() < 1e-5);
        assert_eq!(
            panel_in_tracking(&layer(LayerSpace::Rig), &head, &world).position,
            Vec3::new(0.0, 0.0, -1.0)
        );
        let in_world = panel_in_tracking(&layer(LayerSpace::World), &head, &world);
        assert!((in_world.position - Vec3::new(-5.0, 0.0, -1.0)).length() < 1e-5);
    }

    #[test]
    fn a_panel_at_two_meters_lands_at_its_projection() {
        let proj = regia_core::glam::camera::rh::proj::directx::perspective(1.5, 1.0, 0.1, 100.0);
        let pose = Pose {
            position: Vec3::new(0.0, 0.0, -2.0),
            orientation: Quat::IDENTITY,
        };
        let c = panel_point_clip(proj, &pose, Vec2::new(1.0, 0.0));
        let tan = (1.5_f32 * 0.5).tan();
        assert!((c.x / c.w - 0.5 / tan).abs() < 1e-5);
    }

    #[test]
    fn solid_colors_decode_from_srgb() {
        assert_eq!(srgb_to_linear(0), 0.0);
        assert!((srgb_to_linear(255) - 1.0).abs() < 1e-6);
        assert!((srgb_to_linear(188) - 0.5).abs() < 0.01);
    }
}
