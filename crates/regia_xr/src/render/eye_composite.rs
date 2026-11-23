// SPDX-License-Identifier: MPL-2.0
//! The per-eye composite (spec 58 §§ 2.6, 2.8): the comfort vignette after the tonemap, and the
//! hidden area painted black so a mirror or a capture never shows what no lens does.
//!
//! A [`FramePass`] at [`GraphSlot::Overlay`]: it records into the eye's own frame and does nothing
//! in any other view. With no vignette and no mask it records nothing, so such an eye is
//! byte-identical to one without the pass.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use regia_render::{FramePass, FramePassContext, GraphSlot, stereo_eye_of_view};

/// One eye's composite inputs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EyeComposite {
    /// Vignette darkening now, 0 to 1 (0: no vignette).
    pub strength: f32,
    /// Angle off the view axis where the vignette starts, radians.
    pub inner: f32,
    /// Angle off the view axis where it is full, radians.
    pub outer: f32,
    /// Tangents of the eye's left, right, down and up edges.
    pub tangents: [f32; 4],
    /// Identifies the hidden-area mesh, for the uploaded copy.
    pub mask_generation: u64,
    /// The hidden-area triangles in clip space (empty: no mask).
    pub mask_clip: Arc<Vec<[f32; 4]>>,
}

/// The vignette's darkening at a point of the view plane `(tan_x, tan_y)`: what `fs_vignette`
/// computes, for tools and tests.
pub fn vignette_alpha(composite: &EyeComposite, tan_x: f32, tan_y: f32) -> f32 {
    let angle = (tan_x * tan_x + tan_y * tan_y).sqrt().atan();
    let span = (composite.outer - composite.inner).max(1e-6);
    let t = ((angle - composite.inner) / span).clamp(0.0, 1.0);
    composite.strength * (t * t * (3.0 - 2.0 * t))
}

/// The composite inputs, shared between the frame system that fills them and the registered pass.
#[derive(Clone, Default)]
pub struct EyeCompositeInputs(Arc<Mutex<[EyeComposite; 2]>>);

impl EyeCompositeInputs {
    /// Sets both eyes' inputs for the next frame.
    pub fn set(&self, eyes: [EyeComposite; 2]) {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = eyes;
    }

    fn get(&self, eye: usize) -> EyeComposite {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)[eye].clone()
    }
}

struct Pipelines {
    vignette: wgpu::RenderPipeline,
    hidden: wgpu::RenderPipeline,
}

struct Shared {
    module: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
    params_bgl: wgpu::BindGroupLayout,
}

/// The composite's GPU side: per-format pipelines and per-eye buffers.
#[derive(Default)]
pub struct EyeCompositor {
    shared: Option<Shared>,
    pipelines: HashMap<wgpu::TextureFormat, Pipelines>,
    params: [Option<(wgpu::Buffer, wgpu::BindGroup)>; 2],
    masks: [Option<(u64, wgpu::Buffer, u32)>; 2],
}

const PARAMS_SIZE: u64 = 32;

fn pipeline(
    d: &wgpu::Device,
    shared: &Shared,
    format: wgpu::TextureFormat,
    label: &str,
    vertex: (&str, &[Option<wgpu::VertexBufferLayout<'_>>]),
    fragment: &str,
    blend: Option<wgpu::BlendState>,
) -> wgpu::RenderPipeline {
    d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&shared.layout),
        vertex: wgpu::VertexState {
            module: &shared.module,
            entry_point: Some(vertex.0),
            buffers: vertex.1,
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shared.module,
            entry_point: Some(fragment),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend,
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
}

impl EyeCompositor {
    fn ensure(&mut self, d: &wgpu::Device, format: wgpu::TextureFormat) {
        let shared = self.shared.get_or_insert_with(|| {
            let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("xr eye composite"),
                source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/xr_eye_composite.wgsl").into()),
            });
            let params_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("xr eye composite"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(PARAMS_SIZE),
                    },
                    count: None,
                }],
            });
            let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("xr eye composite"),
                bind_group_layouts: &[Some(&params_bgl)],
                immediate_size: 0,
            });
            Shared {
                module,
                layout,
                params_bgl,
            }
        });
        let shared = &*shared;
        self.pipelines.entry(format).or_insert_with(|| Pipelines {
            vignette: pipeline(
                d,
                shared,
                format,
                "xr vignette",
                ("vs_full", &[]),
                "fs_vignette",
                Some(wgpu::BlendState::ALPHA_BLENDING),
            ),
            hidden: pipeline(
                d,
                shared,
                format,
                "xr hidden area black",
                (
                    "vs_hidden",
                    &[Some(wgpu::VertexBufferLayout {
                        array_stride: 16,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x4],
                    })],
                ),
                "fs_hidden",
                None,
            ),
        });
    }

    /// Records `composite` for `eye` over `color`. `write` stages buffer data before the
    /// recorded commands run (the frame's queue in a pass).
    pub fn record(
        &mut self,
        d: &wgpu::Device,
        write: &dyn Fn(&wgpu::Buffer, &[u8]),
        encoder: &mut wgpu::CommandEncoder,
        (color, format): (&wgpu::TextureView, wgpu::TextureFormat),
        eye: usize,
        composite: &EyeComposite,
    ) {
        let vignette = composite.strength > 0.0;
        let mask = !composite.mask_clip.is_empty();
        if !vignette && !mask {
            return;
        }
        self.ensure(d, format);
        let (Some(shared), Some(pipes)) = (self.shared.as_ref(), self.pipelines.get(&format)) else {
            return;
        };
        let (params, group) = self.params[eye].get_or_insert_with(|| {
            let buffer = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("xr eye composite"),
                size: PARAMS_SIZE,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("xr eye composite"),
                layout: &shared.params_bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                }],
            });
            (buffer, group)
        });
        let [left, right, down, up] = composite.tangents;
        let values = [
            composite.strength,
            composite.inner,
            composite.outer,
            0.0,
            left,
            right,
            down,
            up,
        ];
        write(params, bytemuck::cast_slice(&values));
        if mask
            && self.masks[eye]
                .as_ref()
                .is_none_or(|(generation, ..)| *generation != composite.mask_generation)
        {
            let bytes = bytemuck::cast_slice::<[f32; 4], u8>(&composite.mask_clip);
            let buffer = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("xr hidden area black"),
                size: bytes.len() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            write(&buffer, bytes);
            let count = composite.mask_clip.len() as u32;
            self.masks[eye] = Some((composite.mask_generation, buffer, count));
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("xr eye composite"),
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
        pass.set_bind_group(0, &*group, &[]);
        if vignette {
            pass.set_pipeline(&pipes.vignette);
            pass.draw(0..3, 0..1);
        }
        if mask && let Some((_, buffer, count)) = self.masks[eye].as_ref() {
            pass.set_pipeline(&pipes.hidden);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..*count, 0..1);
        }
    }
}

/// The composite as a registered frame pass.
pub struct EyeCompositePass {
    inputs: EyeCompositeInputs,
    compositor: EyeCompositor,
}

/// The pass's name in `render.graph`.
pub const EYE_COMPOSITE_PASS: &str = "xr_eye_composite";

impl EyeCompositePass {
    /// A pass reading `inputs`.
    pub fn new(inputs: EyeCompositeInputs) -> Self {
        EyeCompositePass {
            inputs,
            compositor: EyeCompositor::default(),
        }
    }
}

impl FramePass for EyeCompositePass {
    fn name(&self) -> &str {
        EYE_COMPOSITE_PASS
    }

    fn slot(&self) -> GraphSlot {
        GraphSlot::Overlay
    }

    fn reads(&self) -> &[&'static str] {
        &["output"]
    }

    fn writes(&self) -> &[&'static str] {
        &["output"]
    }

    fn record(&mut self, ctx: &mut FramePassContext<'_>) -> Result<(), String> {
        let Some(eye) = stereo_eye_of_view(ctx.texture_view) else {
            return Ok(());
        };
        let composite = self.inputs.get(eye);
        let device: &wgpu::Device = ctx.device;
        let (color, format) = (ctx.color, ctx.color_format);
        let encoder = &mut *ctx.encoder;
        // Staged first: the context's queue and its encoder cannot be borrowed together.
        let staged = std::cell::RefCell::new(Vec::new());
        let write = |buffer: &wgpu::Buffer, bytes: &[u8]| {
            staged.borrow_mut().push((buffer.clone(), bytes.to_vec()));
        };
        self.compositor
            .record(device, &write, encoder, (color, format), eye, &composite);
        for (buffer, bytes) in staged.into_inner() {
            ctx.write_buffer(&buffer, 0, &bytes);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composite() -> EyeComposite {
        EyeComposite {
            strength: 0.8,
            inner: 0.5,
            outer: 1.0,
            tangents: [-1.0, 1.0, -1.0, 1.0],
            ..EyeComposite::default()
        }
    }

    #[test]
    fn the_vignette_leaves_the_center_and_reaches_full_at_the_outer_angle() {
        let c = composite();
        assert_eq!(vignette_alpha(&c, 0.0, 0.0), 0.0);
        assert_eq!(vignette_alpha(&c, 0.5_f32.tan() - 1e-3, 0.0), 0.0);
        assert!((vignette_alpha(&c, 1.0_f32.tan() + 1e-3, 0.0) - 0.8).abs() < 1e-6);
        let mid = vignette_alpha(&c, 0.75_f32.tan(), 0.0);
        assert!((mid - 0.4).abs() < 1e-3, "{mid}");
    }

    #[test]
    fn the_vignette_is_round_in_angle_not_in_screen_axes() {
        let c = composite();
        let tan = 0.8_f32.tan();
        let axis = vignette_alpha(&c, tan, 0.0);
        let diagonal = vignette_alpha(
            &c,
            tan * std::f32::consts::FRAC_1_SQRT_2,
            tan * std::f32::consts::FRAC_1_SQRT_2,
        );
        assert!((axis - diagonal).abs() < 1e-5);
    }

    #[test]
    fn an_unset_pass_reads_empty_inputs() {
        let inputs = EyeCompositeInputs::default();
        assert_eq!(inputs.get(1), EyeComposite::default());
        inputs.set([composite(), EyeComposite::default()]);
        assert_eq!(inputs.get(0).strength, 0.8);
    }
}
