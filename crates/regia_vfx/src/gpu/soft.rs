// SPDX-License-Identifier: MPL-2.0
//! The low-resolution soft-particle layer ([`super::SoftParticleRes`]).
//!
//! The soft, low-frequency alpha particles (dust, smoke, billows: [`crate::scene::is_soft_emitter`])
//! are mostly overdraw: large, faint, overlapping quads. They render into an offscreen layer at
//! 1/2 or 1/4 of the frame size, with the same pipelines and the full-resolution scene depth
//! sampled for the soft and spherical fades, and are composited back with a depth-aware
//! bilateral upsample (`soft_half_res.wgsl`) inside the frame's transparent pass. Everything
//! sharp (sparks, tracers, muzzle cores, fire bodies, grains) stays full resolution. The frame's
//! soft-pass mode (`FrameGpu::fire_color.w`, bits 1..) splits the sorted alpha list between the
//! two passes in the vertex shader.
//!
//! Provenance: Cantlay, "High-Speed, Off-Screen Particles", GPU Gems 3 ch. 23 (2007); Kopf et
//! al., "Joint Bilateral Upsampling", SIGGRAPH 2007.

use std::collections::HashMap;

use super::{VfxError, VfxGpu};

/// Relative linear-depth tolerance of the upsample: a tap whose scene depth differs from the
/// pixel's by 5 % keeps e⁻¹ of its weight.
const DEPTH_TOLERANCE: f32 = 20.0;

/// The offscreen layer and the composite pipeline state.
#[derive(Default)]
pub(super) struct SoftLayer {
    /// `(width, height)` of the current layer.
    size: (u32, u32),
    color: Option<wgpu::TextureView>,
    depth: Option<wgpu::TextureView>,
    params: Option<wgpu::Buffer>,
    bgl: Option<wgpu::BindGroupLayout>,
    layout: Option<wgpu::PipelineLayout>,
    module: Option<wgpu::ShaderModule>,
    pipes: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    bind_group: Option<wgpu::BindGroup>,
    /// The views [`Self::bind_group`] was built from (layer colour, scene depth): rebuilt only
    /// when one changes, so a steady frame creates no bind group.
    bind_views: Option<(wgpu::TextureView, wgpu::TextureView)>,
    /// The layer holds this frame's soft particles (recorded and not yet composited).
    ready: bool,
}

/// Layer size for a `width × height` frame at divisor `div` (at least 1 × 1).
pub(super) fn layer_size(width: u32, height: u32, div: u32) -> (u32, u32) {
    let div = div.max(1);
    (
        width.max(1).div_ceil(div).max(1),
        height.max(1).div_ceil(div).max(1),
    )
}

pub(super) const LAYER_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

impl VfxGpu {
    /// Whether the host composites the soft layer this frame. The runtime layer enables it on
    /// HDR frames (the layer is `Rgba16Float`, pre-exposure, like the HDR target); hosts that
    /// draw into a display target leave it off and everything draws full resolution.
    pub fn set_soft_pass(&mut self, on: bool) {
        self.soft_pass = on;
    }

    /// The soft layer is used this frame: the profile asks for a reduced resolution and the
    /// host enabled the pass ([`Self::set_soft_pass`]).
    pub fn soft_pass_active(&self) -> bool {
        self.soft_layer_enabled() && self.soft_live
    }

    /// The host and the profile use the soft layer, whether or not soft particles are alive:
    /// its targets, bind group and composite pipeline are kept ready, so the first dust or
    /// smoke spawn creates no GPU object mid-play (`specs/27` § 2.4).
    fn soft_layer_enabled(&self) -> bool {
        self.soft_pass && self.soft_res != super::SoftParticleRes::Full
    }

    /// Records the soft layer: clears it and draws the soft emitters of the sorted alpha list at
    /// the profile's fraction of `width × height`. Call after [`Self::simulate`] on the same
    /// encoder, with the scene depth already bound ([`Self::set_depth_view`]). No-op when the
    /// soft pass is off.
    ///
    /// # Errors
    ///
    /// A pipeline could not be built.
    pub fn record_soft_pass(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        width: u32,
        height: u32,
    ) -> Result<(), VfxError> {
        self.soft.ready = false;
        if !self.soft_layer_enabled() {
            return Ok(());
        }
        let _span = tracing::trace_span!("vfx_soft_layer").entered();
        // Targets, layout and bind group follow the frame size and the scene depth, not the
        // particles: built on the first enabled frame (warm-up), kept while nothing soft lives.
        let size = layer_size(width, height, self.soft_res.divisor());
        super::check_texture_extent(size.0, size.1, self.device.limits().max_texture_dimension_2d)?;
        self.ensure_soft_targets(size);
        self.ensure_soft_composite_layout();
        // The particle pipelines for the layer's format (the full-resolution lists build the
        // HDR target's; the same set when that is `Rgba16Float`).
        self.pipes_for(LAYER_FORMAT)?;
        let (Some(color), Some(depth)) = (self.soft.color.clone(), self.soft.depth.clone()) else {
            return Ok(());
        };
        let scene_depth = self
            .bound_depth
            .clone()
            .unwrap_or_else(|| self.dummy_depth.clone());
        let views = (color.clone(), scene_depth.clone());
        let stale = self.soft.bind_group.is_none() || self.soft.bind_views.as_ref() != Some(&views);
        if stale && let (Some(bgl), Some(params)) = (&self.soft.bgl, &self.soft.params) {
            self.soft.bind_views = Some(views);
            self.soft.bind_group = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vfx soft composite bg"),
                layout: bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&color),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&scene_depth),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: params.as_entire_binding(),
                    },
                ],
            }));
        }
        if !self.soft_live {
            return Ok(());
        }
        // Composite inputs: this layer and the full-resolution scene depth.
        let near = self.frame_cpu.cam_pos_near[3].max(1e-4);
        let far = self.frame_cpu.params[0].max(near * 2.0);
        if let Some(params) = &self.soft.params {
            self.queue.write_buffer(
                params,
                0,
                bytemuck::cast_slice(&[near, far, DEPTH_TOLERANCE, 0.0f32]),
            );
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vfx soft layer"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth,
                    // The particles sample the full-resolution scene depth for their fades and
                    // occlusion; this attachment only satisfies the pipeline (cleared to far).
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_viewport(0.0, 0.0, size.0 as f32, size.1 as f32, 0.0, 1.0);
            self.draw_alpha_half_res(&mut pass, LAYER_FORMAT)?;
        }
        self.soft.ready = true;
        Ok(())
    }

    /// Composites the soft layer recorded this frame over the colour target of `pass` (format
    /// `format`; the pass's depth attachment is the scene depth, read-only). No-op when no layer
    /// was recorded, except that an enabled layer builds its composite pipeline for `format` on
    /// the first call (warm-up), not on the first soft spawn.
    ///
    /// # Errors
    ///
    /// The composite pipeline could not be built.
    pub fn draw_soft_composite(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        format: wgpu::TextureFormat,
    ) -> Result<(), VfxError> {
        if !self.soft_layer_enabled() {
            return Ok(());
        }
        let pipe = self.soft_composite_pipe(format)?;
        if !self.soft.ready {
            return Ok(());
        }
        let Some(bg) = self.soft.bind_group.clone() else {
            return Ok(());
        };
        pass.set_pipeline(&pipe);
        pass.set_bind_group(0, Some(&bg), &[]);
        pass.draw(0..3, 0..1);
        Ok(())
    }

    fn ensure_soft_targets(&mut self, size: (u32, u32)) {
        if self.soft.size == size && self.soft.color.is_some() {
            return;
        }
        let make = |label: &str, format: wgpu::TextureFormat, usage: wgpu::TextureUsages| {
            self.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: size.0,
                        height: size.1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default())
        };
        self.soft.color = Some(make(
            "vfx soft layer",
            LAYER_FORMAT,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        ));
        self.soft.depth = Some(make(
            "vfx soft layer depth",
            wgpu::TextureFormat::Depth32Float,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
        ));
        self.soft.size = size;
    }

    fn ensure_soft_composite_layout(&mut self) {
        if self.soft.bgl.is_some() {
            return;
        }
        let d = &self.device;
        let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx soft composite bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vfx soft composite layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vfx soft composite"),
            source: wgpu::ShaderSource::Wgsl(include_str!("soft_half_res.wgsl").into()),
        });
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx soft composite params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.soft.bgl = Some(bgl);
        self.soft.layout = Some(layout);
        self.soft.module = Some(module);
        self.soft.params = Some(params);
    }

    fn soft_composite_pipe(&mut self, format: wgpu::TextureFormat) -> Result<wgpu::RenderPipeline, VfxError> {
        if let Some(p) = self.soft.pipes.get(&format) {
            return Ok(p.clone());
        }
        let source = self.soft_composite_source()?;
        let pipe = source.build(format)?;
        self.soft.pipes.insert(format, pipe.clone());
        Ok(pipe)
    }

    /// The composite's layout and module (built here once), for [`SoftCompositeSource::build`].
    pub(super) fn soft_composite_source(&mut self) -> Result<SoftCompositeSource, VfxError> {
        self.ensure_soft_composite_layout();
        let (Some(layout), Some(module)) = (&self.soft.layout, &self.soft.module) else {
            return Err(VfxError::Shader("soft composite layout missing".into()));
        };
        Ok(SoftCompositeSource {
            device: self.device.clone(),
            layout: layout.clone(),
            module: module.clone(),
        })
    }

    /// The soft layer is on in the profile (whatever the host's frame): the formats it uses
    /// are worth warming ([`Self::warm`]).
    pub(super) fn soft_layer_in_profile(&self) -> bool {
        self.soft_res != super::SoftParticleRes::Full
    }

    /// Whether the composite pipeline of `format` is built.
    pub(super) fn has_soft_composite(&self, format: wgpu::TextureFormat) -> bool {
        self.soft.pipes.contains_key(&format)
    }

    /// Keeps a composite pipeline a warm built (the first one wins).
    pub(super) fn insert_soft_composite(&mut self, format: wgpu::TextureFormat, pipe: wgpu::RenderPipeline) {
        self.soft.pipes.entry(format).or_insert(pipe);
    }
}

/// The format-independent inputs of the composite pipeline (shared handles), so a warm job
/// compiles it without holding the context.
#[derive(Clone)]
pub(super) struct SoftCompositeSource {
    device: regia_render::TrackedDevice,
    layout: wgpu::PipelineLayout,
    module: wgpu::ShaderModule,
}

impl SoftCompositeSource {
    /// The composite pipeline into a `format` target.
    pub(super) fn build(&self, format: wgpu::TextureFormat) -> Result<wgpu::RenderPipeline, VfxError> {
        let d = &self.device;
        let scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
        let premult = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        };
        let pipe = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vfx soft composite"),
            layout: Some(&self.layout),
            vertex: wgpu::VertexState {
                module: &self.module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &self.module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: premult,
                        alpha: premult,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            // Drawn inside the transparent pass, whose read-only depth is the scene depth.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: regia_render::pipeline_cache::cache_for(d).as_deref(),
        });
        if let Some(err) = regia_core::tasks::block_on(scope.pop()) {
            return Err(VfxError::Shader(err.to_string()));
        }
        Ok(pipe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_size_rounds_up_and_never_vanishes() {
        assert_eq!(layer_size(2560, 1440, 2), (1280, 720));
        assert_eq!(layer_size(2560, 1440, 4), (640, 360));
        assert_eq!(layer_size(1281, 721, 2), (641, 361));
        assert_eq!(layer_size(1, 1, 4), (1, 1));
        assert_eq!(layer_size(0, 0, 2), (1, 1));
    }

    /// ADR-026: no Function-storage array with an explicit layout (the old indexed triangle).
    #[test]
    fn upsample_shader_has_no_function_layout_errors() {
        let found = regia_render::wgsl_function_layout_errors(
            "soft_half_res.wgsl",
            include_str!("soft_half_res.wgsl"),
        )
        .expect("soft_half_res emits");
        assert!(found.is_empty(), "{found:?}");
    }
}
