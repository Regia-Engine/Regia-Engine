// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_gizmos_render/src/{lib,pipeline_3d}.rs. Copyright (c) Bevy contributors,
// dual-licensed MIT OR Apache-2.0; used here under the MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//! The gizmo pass: instanced thick lines and strip joints drawn over the tonemapped frame.
//!
//! [`crate::FrameRenderer::render_into`] records it after the scene, post stack and translucent
//! layers, before the UI overlay. Bevy draws gizmos in the main 3D phase with a depth
//! attachment; Regia Engine draws them into the final LDR target and tests the scene depth in the
//! fragment shader, so one path serves every quality preset (native, scaled, TAAU, MSAA).

use crate::frame_work::counted::*;
use std::collections::HashMap;

use naga_oil::compose::ShaderDefValue;
use regia_core::math::Mat4;

use super::config::{GizmoLineJoint, GizmoLineStyle};
use super::extract::{GizmoBatch, GizmoFrame, GizmoJoint, GizmoLabelBatch, GizmoLabelVertex, GizmoSegment};
use crate::RenderError;
use crate::device::{TrackedDevice, TrackedQueue};

const SHADER: &str = include_str!("../../shaders/gizmos.wgsl");
/// Dynamic uniform offsets must be multiples of this (WebGPU's minimum, every backend).
const BATCH_STRIDE: u64 = 256;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewUniform {
    clip_from_world: [[f32; 4]; 4],
    view_from_clip: [[f32; 4]; 4],
    world_from_clip: [[f32; 4]; 4],
    viewport: [f32; 4],
    flags: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct LineUniform {
    world_from_local: [[f32; 4]; 4],
    line_width: f32,
    depth_bias: f32,
    joints_resolution: u32,
    on_top: u32,
    gap_scale: f32,
    line_scale: f32,
    animation_offset: f32,
    pad: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Kind {
    Solid,
    Dotted,
    Dashed,
    Bevel,
    Miter,
    Round,
    /// Debug-label glyph quads from a font atlas page (ADR-7254).
    Label,
}

impl Kind {
    fn entry_points(self) -> (&'static str, &'static str) {
        match self {
            Kind::Solid => ("vertex", "fragment_solid"),
            Kind::Dotted => ("vertex", "fragment_dotted"),
            Kind::Dashed => ("vertex", "fragment_dashed"),
            Kind::Bevel => ("vertex_bevel", "fragment_joint"),
            Kind::Miter => ("vertex_miter", "fragment_joint"),
            Kind::Round => ("vertex_round", "fragment_joint"),
            Kind::Label => ("vertex_label", "fragment_label"),
        }
    }

    fn is_joint(self) -> bool {
        matches!(self, Kind::Bevel | Kind::Miter | Kind::Round)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct PipelineKey {
    format: wgpu::TextureFormat,
    kind: Kind,
    perspective: bool,
    multisampled_depth: bool,
}

/// Where and how the pass draws.
pub(crate) struct GizmoTarget<'a> {
    /// The final colour attachment (LDR, after tonemap).
    pub view: &'a wgpu::TextureView,
    /// Its format.
    pub format: wgpu::TextureFormat,
    /// Viewport rectangle in target pixels (x, y, w, h).
    pub viewport: [f32; 4],
    /// The frame's scene depth (any size; sampled by UV inside the viewport).
    pub depth: &'a wgpu::TextureView,
    /// `depth` is multisampled.
    pub multisampled_depth: bool,
    /// Unjittered view-projection of the frame (standard depth 0..1).
    pub clip_from_world: Mat4,
    /// Unjittered projection of the frame.
    pub clip_from_view: Mat4,
}

/// GPU state of the gizmo pass (pipelines are built on first use per variant).
#[derive(Default)]
pub(crate) struct GizmoRenderer {
    layouts: Option<Layouts>,
    modules: HashMap<(bool, bool), wgpu::ShaderModule>,
    pipelines: HashMap<PipelineKey, wgpu::RenderPipeline>,
    view_buffer: Option<wgpu::Buffer>,
    batch_buffer: Option<(wgpu::Buffer, u64)>,
    segment_buffer: Option<(wgpu::Buffer, u64)>,
    joint_buffer: Option<(wgpu::Buffer, u64)>,
    /// Instance lists reused across frames. Capacity stays when a later frame is smaller.
    segment_scratch: Vec<GizmoSegment>,
    joint_scratch: Vec<GizmoJoint>,
    /// Batch uniform bytes, reused the same way.
    uniform_scratch: Vec<u8>,
    /// View bind group, kept while the depth view and its sample count match.
    view_group: Option<(wgpu::TextureView, bool, wgpu::BindGroup)>,
    /// Batch bind group, kept while the uniform buffer is the same allocation.
    batch_group: Option<(wgpu::Buffer, wgpu::BindGroup)>,
    /// Debug-label vertices of every label batch (ADR-7254).
    label_buffer: Option<(wgpu::Buffer, u64)>,
    /// Label vertex list reused across frames.
    label_scratch: Vec<GizmoLabelVertex>,
    /// Atlas pages on the GPU: page index → (generation, its texture's bind group).
    label_pages: HashMap<u32, (u64, wgpu::BindGroup)>,
    /// The pages' bilinear sampler.
    label_sampler: Option<wgpu::Sampler>,
}

/// The buffer in `slot`, grown (to a power of two) when it is smaller than `bytes`. Steady frames
/// reuse it, so the pass allocates nothing once the gizmo count settles.
fn ensure_buffer(
    device: &wgpu::Device,
    slot: &mut Option<(wgpu::Buffer, u64)>,
    bytes: u64,
    usage: wgpu::BufferUsages,
    label: &'static str,
) -> wgpu::Buffer {
    match slot {
        Some((buffer, size)) if *size >= bytes => buffer.clone(),
        _ => {
            let size = bytes.next_power_of_two().max(4096);
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            *slot = Some((buffer.clone(), size));
            buffer
        }
    }
}

struct Layouts {
    view: [wgpu::BindGroupLayout; 2],
    batch: wgpu::BindGroupLayout,
    pipeline: [wgpu::PipelineLayout; 2],
    /// A label's atlas page and sampler (group 2).
    label: wgpu::BindGroupLayout,
    /// The label pipelines: view, batch and page groups.
    label_pipeline: [wgpu::PipelineLayout; 2],
}

fn view_layout(device: &wgpu::Device, multisampled: bool) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("gizmo view"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<ViewUniform>() as u64),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled,
                },
                count: None,
            },
        ],
    })
}

impl Layouts {
    fn new(device: &wgpu::Device) -> Self {
        let view = [view_layout(device, false), view_layout(device, true)];
        let batch = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gizmo batch"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(std::mem::size_of::<LineUniform>() as u64),
                },
                count: None,
            }],
        });
        let pipeline = [0, 1].map(|i| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("gizmo"),
                bind_group_layouts: &[Some(&view[i]), Some(&batch)],
                immediate_size: 0,
            })
        });
        let label = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gizmo label page"),
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
        let label_pipeline = [0, 1].map(|i| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("gizmo label"),
                bind_group_layouts: &[Some(&view[i]), Some(&batch), Some(&label)],
                immediate_size: 0,
            })
        });
        Self {
            view,
            batch,
            pipeline,
            label,
            label_pipeline,
        }
    }
}

const LABEL_ATTRIBUTES: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
    0 => Float32x3,
    1 => Float32x2,
    2 => Float32x4,
];

const SEGMENT_ATTRIBUTES: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
    0 => Float32x3,
    1 => Float32x3,
    2 => Float32x4,
    3 => Float32x4,
];

const JOINT_ATTRIBUTES: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
    0 => Float32x3,
    1 => Float32x3,
    2 => Float32x3,
    3 => Float32x4,
];

/// Shader defs of one module variant.
pub(crate) fn shader_defs(perspective: bool, multisampled_depth: bool) -> HashMap<String, ShaderDefValue> {
    let mut defs = HashMap::new();
    if perspective {
        defs.insert("PERSPECTIVE".to_string(), ShaderDefValue::Bool(true));
    }
    if multisampled_depth {
        defs.insert("DEPTH_MULTISAMPLED".to_string(), ShaderDefValue::Bool(true));
    }
    defs
}

/// The gizmo shader source (for the shader validation gate).
pub(crate) const fn shader_source() -> &'static str {
    SHADER
}

fn line_kind(style: GizmoLineStyle) -> Kind {
    match style {
        GizmoLineStyle::Solid => Kind::Solid,
        GizmoLineStyle::Dotted => Kind::Dotted,
        GizmoLineStyle::Dashed { .. } => Kind::Dashed,
    }
}

fn joint_kind(joints: GizmoLineJoint) -> Option<(Kind, u32)> {
    match joints {
        GizmoLineJoint::None => None,
        GizmoLineJoint::Bevel => Some((Kind::Bevel, 3)),
        GizmoLineJoint::Miter => Some((Kind::Miter, 6)),
        GizmoLineJoint::Round(resolution) => (resolution > 0).then_some((Kind::Round, resolution * 3)),
    }
}

impl GizmoRenderer {
    /// Copies every batch's instances into the scratch `Vec`s, keeping their capacity.
    fn gather_instances(&mut self, batches: &[&GizmoBatch]) {
        let segments = batches.iter().map(|batch| batch.segments.len()).sum();
        let joints = batches.iter().map(|batch| batch.joints.len()).sum();
        self.segment_scratch.clear();
        self.joint_scratch.clear();
        self.segment_scratch.reserve(segments);
        self.joint_scratch.reserve(joints);
        for batch in batches {
            self.segment_scratch.extend(batch.segments.iter().copied());
            self.joint_scratch.extend(batch.joints.iter().copied());
        }
    }

    fn module(
        &mut self,
        device: &wgpu::Device,
        perspective: bool,
        multisampled: bool,
    ) -> Result<wgpu::ShaderModule, RenderError> {
        if let Some(module) = self.modules.get(&(perspective, multisampled)) {
            return Ok(module.clone());
        }
        let naga_module =
            crate::shader_lib::compose_source(SHADER, "gizmos.wgsl", shader_defs(perspective, multisampled))?;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gizmos.wgsl"),
            source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(naga_module)),
        });
        self.modules.insert((perspective, multisampled), module.clone());
        Ok(module)
    }

    fn pipeline(
        &mut self,
        device: &TrackedDevice,
        key: PipelineKey,
    ) -> Result<wgpu::RenderPipeline, RenderError> {
        if let Some(pipeline) = self.pipelines.get(&key) {
            return Ok(pipeline.clone());
        }
        let module = self.module(device, key.perspective, key.multisampled_depth)?;
        let layouts = self.layouts.get_or_insert_with(|| Layouts::new(device));
        let (vs, fs) = key.kind.entry_points();
        let label = key.kind == Kind::Label;
        let (stride, attributes, step_mode): (u64, &[wgpu::VertexAttribute], _) = if label {
            (
                std::mem::size_of::<GizmoLabelVertex>() as u64,
                &LABEL_ATTRIBUTES,
                wgpu::VertexStepMode::Vertex,
            )
        } else if key.kind.is_joint() {
            (
                std::mem::size_of::<GizmoJoint>() as u64,
                &JOINT_ATTRIBUTES,
                wgpu::VertexStepMode::Instance,
            )
        } else {
            (
                std::mem::size_of::<GizmoSegment>() as u64,
                &SEGMENT_ATTRIBUTES,
                wgpu::VertexStepMode::Instance,
            )
        };
        let layout = if label {
            &layouts.label_pipeline[usize::from(key.multisampled_depth)]
        } else {
            &layouts.pipeline[usize::from(key.multisampled_depth)]
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("gizmo"),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some(vs),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: stride,
                    step_mode,
                    attributes,
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some(fs),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: key.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        });
        self.pipelines.insert(key, pipeline.clone());
        Ok(pipeline)
    }

    /// Records the gizmo pass for `frame` into `encoder`. Does nothing for an empty frame.
    #[tracing::instrument(skip_all)]
    pub(crate) fn record(
        &mut self,
        device: &TrackedDevice,
        queue: &TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        frame: &GizmoFrame,
        target: &GizmoTarget<'_>,
    ) -> Result<(), RenderError> {
        let batches: Vec<_> = frame.batches.iter().filter(|b| !b.is_empty()).collect();
        let labels: Vec<&GizmoLabelBatch> = frame
            .labels
            .iter()
            .filter(|l| !l.vertices.is_empty() && l.page.1.is_valid())
            .collect();
        if batches.is_empty() && labels.is_empty() {
            return Ok(());
        }
        // Pipelines first (they borrow self mutably).
        let mut draws = Vec::with_capacity(batches.len());
        for batch in &batches {
            let common = |kind| PipelineKey {
                format: target.format,
                kind,
                perspective: batch.line.perspective,
                multisampled_depth: target.multisampled_depth,
            };
            let line_pipeline = self.pipeline(device, common(line_kind(batch.line.style)))?;
            let joint = match joint_kind(batch.line.joints) {
                Some((kind, vertices)) if !batch.joints.is_empty() => {
                    Some((self.pipeline(device, common(kind))?, vertices))
                }
                _ => None,
            };
            draws.push((line_pipeline, joint));
        }
        let label_pipeline = if labels.is_empty() {
            None
        } else {
            Some(self.pipeline(
                device,
                PipelineKey {
                    format: target.format,
                    kind: Kind::Label,
                    perspective: false,
                    multisampled_depth: target.multisampled_depth,
                },
            )?)
        };

        self.gather_instances(&batches);

        let (view_layout, batch_layout) = {
            let Some(layouts) = self.layouts.as_ref() else {
                return Ok(());
            };
            (
                layouts.view[usize::from(target.multisampled_depth)].clone(),
                layouts.batch.clone(),
            )
        };
        let view_uniform = ViewUniform {
            clip_from_world: target.clip_from_world.to_cols_array_2d(),
            view_from_clip: target.clip_from_view.inverse().to_cols_array_2d(),
            world_from_clip: target.clip_from_world.inverse().to_cols_array_2d(),
            viewport: target.viewport,
            flags: [u32::from(!target.format.is_srgb()), 0, 0, 0],
        };
        let view_buffer = self
            .view_buffer
            .get_or_insert_with(|| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("gizmo view"),
                    size: std::mem::size_of::<ViewUniform>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            })
            .clone();
        queue.write_buffer(&view_buffer, 0, bytemuck::bytes_of(&view_uniform));

        // One uniform slot per line batch, then one per label batch.
        let batch_bytes = BATCH_STRIDE * (batches.len() + labels.len()) as u64;
        let batch_buffer = ensure_buffer(
            device,
            &mut self.batch_buffer,
            batch_bytes,
            wgpu::BufferUsages::UNIFORM,
            "gizmo batches",
        );
        self.uniform_scratch.resize(batch_bytes as usize, 0);
        for (i, batch) in batches.iter().enumerate() {
            let (gap_scale, line_scale) = match batch.line.style {
                GizmoLineStyle::Dashed {
                    gap_scale,
                    line_scale,
                } => (gap_scale, line_scale),
                _ => (1.0, 1.0),
            };
            let uniform = LineUniform {
                world_from_local: batch.world_from_local.to_cols_array_2d(),
                line_width: batch.line.width,
                depth_bias: batch.depth_bias.clamp(-1.0, 1.0),
                joints_resolution: match batch.line.joints {
                    GizmoLineJoint::Round(resolution) => resolution,
                    _ => 0,
                },
                on_top: u32::from(batch.on_top),
                gap_scale: gap_scale.max(f32::EPSILON),
                line_scale: line_scale.max(f32::EPSILON),
                animation_offset: batch.line.animation_offset,
                pad: 0.0,
            };
            let start = i * BATCH_STRIDE as usize;
            self.uniform_scratch[start..start + std::mem::size_of::<LineUniform>()]
                .copy_from_slice(bytemuck::bytes_of(&uniform));
        }
        for (j, label) in labels.iter().enumerate() {
            let uniform = LineUniform {
                world_from_local: Mat4::IDENTITY.to_cols_array_2d(),
                line_width: 1.0,
                depth_bias: 0.0,
                joints_resolution: 0,
                on_top: u32::from(label.on_top),
                gap_scale: 1.0,
                line_scale: 1.0,
                animation_offset: 0.0,
                pad: 0.0,
            };
            let start = (batches.len() + j) * BATCH_STRIDE as usize;
            self.uniform_scratch[start..start + std::mem::size_of::<LineUniform>()]
                .copy_from_slice(bytemuck::bytes_of(&uniform));
        }
        queue.write_buffer(&batch_buffer, 0, &self.uniform_scratch);
        let label_draws = self.prepare_labels(device, queue, &labels);

        // Instance data for every batch in two buffers; each batch draws its own range.
        let segment_buffer = ensure_buffer(
            device,
            &mut self.segment_buffer,
            std::mem::size_of_val(self.segment_scratch.as_slice()) as u64,
            wgpu::BufferUsages::VERTEX,
            "gizmo segments",
        );
        if !self.segment_scratch.is_empty() {
            queue.write_buffer(&segment_buffer, 0, bytemuck::cast_slice(&self.segment_scratch));
        }
        let joint_buffer = (!self.joint_scratch.is_empty()).then(|| {
            let buffer = ensure_buffer(
                device,
                &mut self.joint_buffer,
                std::mem::size_of_val(self.joint_scratch.as_slice()) as u64,
                wgpu::BufferUsages::VERTEX,
                "gizmo joints",
            );
            queue.write_buffer(&buffer, 0, bytemuck::cast_slice(&self.joint_scratch));
            buffer
        });

        let multisampled = target.multisampled_depth;
        let cached_view = self.view_group.as_ref().and_then(|(depth, samples, group)| {
            (depth == target.depth && *samples == multisampled).then(|| group.clone())
        });
        let view_group = if let Some(group) = cached_view {
            group
        } else {
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("gizmo view"),
                layout: &view_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: view_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(target.depth),
                    },
                ],
            });
            self.view_group = Some((target.depth.clone(), multisampled, group.clone()));
            group
        };
        let cached_batch = self
            .batch_group
            .as_ref()
            .and_then(|(buffer, group)| (buffer == &batch_buffer).then(|| group.clone()));
        let batch_group = if let Some(group) = cached_batch {
            group
        } else {
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("gizmo batches"),
                layout: &batch_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &batch_buffer,
                        offset: 0,
                        size: wgpu::BufferSize::new(std::mem::size_of::<LineUniform>() as u64),
                    }),
                }],
            });
            self.batch_group = Some((batch_buffer.clone(), group.clone()));
            group
        };

        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("gizmos"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let [x, y, w, h] = target.viewport;
        pass.set_viewport(x, y, w.max(1.0), h.max(1.0), 0.0, 1.0);
        pass.counted_set_bind_group(0, &view_group, &[]);
        let (mut segment_base, mut joint_base) = (0u32, 0u32);
        for (i, (batch, (line_pipeline, joint))) in batches.iter().zip(&draws).enumerate() {
            let offset = (i as u64 * BATCH_STRIDE) as u32;
            pass.counted_set_bind_group(1, &batch_group, &[offset]);
            let segment_count = batch.segments.len() as u32;
            pass.counted_set_pipeline(line_pipeline);
            pass.set_vertex_buffer(0, segment_buffer.slice(..));
            pass.counted_draw(0..6, segment_base..segment_base + segment_count);
            segment_base += segment_count;
            let joint_count = batch.joints.len() as u32;
            if let (Some((joint_pipeline, vertices)), Some(buffer)) = (joint, joint_buffer.as_ref()) {
                pass.counted_set_pipeline(joint_pipeline);
                pass.set_vertex_buffer(0, buffer.slice(..));
                pass.counted_draw(0..*vertices, joint_base..joint_base + joint_count);
            }
            joint_base += joint_count;
        }
        if let (Some(pipeline), Some((buffer, groups))) = (label_pipeline.as_ref(), label_draws.as_ref()) {
            pass.counted_set_pipeline(pipeline);
            pass.set_vertex_buffer(0, buffer.slice(..));
            let mut base = 0u32;
            for (j, (label, group)) in labels.iter().zip(groups).enumerate() {
                let offset = ((batches.len() + j) as u64 * BATCH_STRIDE) as u32;
                pass.counted_set_bind_group(1, &batch_group, &[offset]);
                pass.counted_set_bind_group(2, group, &[]);
                let count = label.vertices.len() as u32;
                pass.counted_draw(base..base + count, 0..1);
                base += count;
            }
        }
        Ok(())
    }

    /// Uploads the label vertices and every atlas page whose generation moved; returns the vertex
    /// buffer and each label batch's page group, or `None` without labels (ADR-7254).
    fn prepare_labels(
        &mut self,
        device: &TrackedDevice,
        queue: &TrackedQueue,
        labels: &[&GizmoLabelBatch],
    ) -> Option<(wgpu::Buffer, Vec<wgpu::BindGroup>)> {
        if labels.is_empty() {
            return None;
        }
        let layout = self.layouts.as_ref()?.label.clone();
        self.label_scratch.clear();
        for label in labels {
            self.label_scratch.extend_from_slice(&label.vertices);
        }
        let buffer = ensure_buffer(
            device,
            &mut self.label_buffer,
            std::mem::size_of_val(self.label_scratch.as_slice()) as u64,
            wgpu::BufferUsages::VERTEX,
            "gizmo labels",
        );
        queue.write_buffer(&buffer, 0, bytemuck::cast_slice(&self.label_scratch));
        let sampler = self
            .label_sampler
            .get_or_insert_with(|| {
                device.create_sampler(&wgpu::SamplerDescriptor {
                    label: Some("gizmo label page"),
                    mag_filter: wgpu::FilterMode::Linear,
                    min_filter: wgpu::FilterMode::Linear,
                    ..Default::default()
                })
            })
            .clone();
        let mut groups = Vec::with_capacity(labels.len());
        for label in labels {
            let (index, page) = (label.page.0, &label.page.1);
            let current = self
                .label_pages
                .get(&index)
                .filter(|(generation, _)| *generation == page.generation)
                .map(|(_, group)| group.clone());
            let group = match current {
                Some(group) => group,
                None => {
                    let size = wgpu::Extent3d {
                        width: page.size,
                        height: page.size,
                        depth_or_array_layers: 1,
                    };
                    let texture = device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("gizmo label page"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::R8Unorm,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &page.texels,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(page.size),
                            rows_per_image: Some(page.size),
                        },
                        size,
                    );
                    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("gizmo label page"),
                        layout: &layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&sampler),
                            },
                        ],
                    });
                    self.label_pages.insert(index, (page.generation, group.clone()));
                    group
                }
            };
            groups.push(group);
        }
        Some((buffer, groups))
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "gizmos.wgsl",
            "GizmoLine",
            LineUniform {
                world_from_local,
                line_width,
                depth_bias,
                joints_resolution,
                on_top,
                gap_scale,
                line_scale,
                animation_offset,
                pad
            }
        ),
        crate::contract_row!(
            "gizmos.wgsl",
            "GizmoView",
            ViewUniform {
                clip_from_world,
                view_from_clip,
                world_from_clip,
                viewport,
                flags
            }
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gizmos::GizmoLineConfig;

    #[test]
    fn uniform_layouts_fit_the_shader() {
        // GizmoView: three mat4 + two vec4 = 224 bytes; GizmoLine: mat4 + eight scalars = 96.
        assert_eq!(std::mem::size_of::<ViewUniform>(), 224);
        assert_eq!(std::mem::size_of::<LineUniform>(), 96);
        assert!(std::mem::size_of::<LineUniform>() as u64 <= BATCH_STRIDE);
        assert_eq!(std::mem::size_of::<GizmoSegment>(), 56);
        assert_eq!(std::mem::size_of::<GizmoJoint>(), 52);
        assert_eq!(std::mem::size_of::<GizmoLabelVertex>(), 36);
    }

    #[test]
    fn every_variant_composes() {
        for perspective in [false, true] {
            for multisampled in [false, true] {
                crate::shader_lib::compose_source(
                    SHADER,
                    "gizmos.wgsl",
                    shader_defs(perspective, multisampled),
                )
                .unwrap_or_else(|err| panic!("gizmos.wgsl ({perspective}, {multisampled}): {err}"));
            }
        }
    }

    #[test]
    fn joint_vertex_counts_match_bevy() {
        assert_eq!(joint_kind(GizmoLineJoint::None), None);
        assert_eq!(joint_kind(GizmoLineJoint::Bevel), Some((Kind::Bevel, 3)));
        assert_eq!(joint_kind(GizmoLineJoint::Miter), Some((Kind::Miter, 6)));
        assert_eq!(joint_kind(GizmoLineJoint::Round(4)), Some((Kind::Round, 12)));
        assert_eq!(joint_kind(GizmoLineJoint::Round(0)), None);
    }

    fn batch_with(n: usize) -> GizmoBatch {
        let segments: Vec<GizmoSegment> = (0..n)
            .map(|i| GizmoSegment {
                a: [i as f32, 0.0, 0.0],
                b: [i as f32, 1.0, 0.0],
                color_a: [1.0; 4],
                color_b: [1.0; 4],
            })
            .collect();
        GizmoBatch {
            world_from_local: Mat4::IDENTITY,
            line: GizmoLineConfig::default(),
            depth_bias: 0.0,
            on_top: false,
            segments: std::sync::Arc::from(segments),
            joints: std::sync::Arc::from(Vec::new()),
        }
    }

    #[test]
    fn audit_g3_scratch_capacity_survives_a_smaller_second_frame() {
        let mut renderer = GizmoRenderer::default();
        let big = batch_with(64);
        let small = batch_with(4);
        renderer.gather_instances(&[&big]);
        let segment_cap = renderer.segment_scratch.capacity();
        let joint_cap = renderer.joint_scratch.capacity();
        assert!(segment_cap >= 64, "{segment_cap}");
        renderer.gather_instances(&[&small]);
        assert_eq!(renderer.segment_scratch.len(), 4);
        assert_eq!(
            renderer.segment_scratch.capacity(),
            segment_cap,
            "a smaller frame dropped the segment scratch ({segment_cap} -> {})",
            renderer.segment_scratch.capacity()
        );
        assert!(
            renderer.joint_scratch.capacity() >= joint_cap,
            "joint scratch shrank"
        );
    }

    /// A second record of the same depth view and batch buffer creates no bind groups.
    ///
    /// Skips unless `VK_ICD_FILENAMES` names lavapipe, so a desktop GPU is never opened.
    #[test]
    fn audit_g3_steady_gizmo_frame_reuses_bind_groups() {
        let icd = std::env::var("VK_ICD_FILENAMES").unwrap_or_default();
        if !icd.contains("lvp") {
            return;
        }
        let device = crate::RenderDevice::headless().unwrap_or_else(|err| panic!("lavapipe device: {err}"));
        assert_eq!(
            device.adapter.get_info().device_type,
            wgpu::DeviceType::Cpu,
            "VK_ICD_FILENAMES selected a non-CPU adapter"
        );
        let color = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("gizmo color"),
            size: wgpu::Extent3d {
                width: 8,
                height: 8,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("gizmo depth"),
            size: wgpu::Extent3d {
                width: 8,
                height: 8,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let frame = GizmoFrame {
            batches: vec![batch_with(4)],
            ..GizmoFrame::default()
        };
        let target = GizmoTarget {
            view: &color_view,
            format: wgpu::TextureFormat::Rgba8Unorm,
            viewport: [0.0, 0.0, 8.0, 8.0],
            depth: &depth_view,
            multisampled_depth: false,
            clip_from_world: Mat4::IDENTITY,
            clip_from_view: Mat4::IDENTITY,
        };
        let tracked = device.tracked();
        let queue = device.tracked_queue();
        let mut renderer = GizmoRenderer::default();
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gizmo") });
        renderer
            .record(&tracked, &queue, &mut encoder, &frame, &target)
            .unwrap_or_else(|err| panic!("first record: {err}"));
        drop(encoder);
        let before = crate::frame_stats::bind_group_creates();
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gizmo") });
        renderer
            .record(&tracked, &queue, &mut encoder, &frame, &target)
            .unwrap_or_else(|err| panic!("second record: {err}"));
        let created = crate::frame_stats::bind_group_creates().saturating_sub(before);
        assert_eq!(created, 0, "a steady gizmo frame created {created} bind groups");
    }
}
