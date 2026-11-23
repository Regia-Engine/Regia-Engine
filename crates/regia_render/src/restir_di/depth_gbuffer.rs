// SPDX-License-Identifier: MPL-2.0
//! The sampler's G-buffer from the scene depth alone (ADR-2470; `shaders/restir_di_debug.wgsl`):
//! linear view depth, a normal reconstructed from the neighbor with the smaller depth step on
//! each axis, one flat material, and camera motion from the previous view-projection.
//!
//! The evaluation pass ([`super::RestirDiDebugPass`]) runs it on every frame; the production
//! hookup (ADR-2470 § 3) can run it after the depth prepass, where the forward renderer has no
//! G-buffer yet. A flat material is enough there: it only shapes the target, the forward pass
//! shades the chosen light with the true material, and the target's support term keeps the
//! estimate unbiased.

use crate::frame_work::counted::*;
use regia_core::glam::{Mat4, Vec3};

use super::RestirDiError;
use crate::TrackedDevice;

const SOURCE: &str = include_str!("../../shaders/restir_di_debug.wgsl");

/// Formats of the outputs: linear depth, normal + roughness, albedo + metallic, motion.
pub const DEPTH_GBUFFER_FORMATS: [wgpu::TextureFormat; 4] = [
    wgpu::TextureFormat::R32Float,
    wgpu::TextureFormat::Rgba16Float,
    wgpu::TextureFormat::Rgba16Float,
    wgpu::TextureFormat::Rgba16Float,
];

/// `DebugParams` in `restir_di_debug.wgsl` and `restir_di_composite.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct DebugParamsGpu {
    pub(crate) world_from_clip: [[f32; 4]; 4],
    pub(crate) prev_clip_from_world: [[f32; 4]; 4],
    pub(crate) eye: [f32; 4],
    pub(crate) forward: [f32; 4],
    pub(crate) viewport: [f32; 4],
    pub(crate) sizes: [f32; 4],
    pub(crate) material: [f32; 4],
}

/// One frame's inputs of [`DepthGBuffer`].
#[derive(Clone, Copy, Debug)]
pub struct DepthGBufferInputs<'a> {
    /// The scene depth: standard 0..1 (1 = nothing), single-sampled, any size (read by UV
    /// inside `viewport`).
    pub depth: &'a wgpu::TextureView,
    /// The view rectangle in `color_size` pixels (x, y, w, h); the outputs are `w × h`.
    pub viewport: [f32; 4],
    /// The size the viewport is measured in.
    pub color_size: (u32, u32),
    /// Unjittered view-projection of the frame.
    pub clip_from_world: Mat4,
    /// Camera position.
    pub eye: [f32; 3],
    /// Unit view direction.
    pub forward: [f32; 3],
    /// The flat material: albedo (rgb) and perceptual roughness.
    pub material: [f32; 4],
    /// Carried in `eye.w` of the parameter block for a composite that shares it (1 elsewhere).
    pub pre_exposure: f32,
}

struct Targets {
    size: (u32, u32),
    views: [wgpu::TextureView; 4],
    _textures: [wgpu::Texture; 4],
    group: Option<(wgpu::TextureView, wgpu::BindGroup)>,
}

/// The depth-only G-buffer stage (see the module docs). Its outputs bind as
/// [`super::RestirInputs`]' `depth`, `normal_roughness`, `albedo_metallic` and `motion`.
pub struct DepthGBuffer {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::ComputePipeline,
    params: wgpu::Buffer,
    params_bytes: DebugParamsGpu,
    targets: Option<Targets>,
    prev_clip_from_world: Option<Mat4>,
    pending: Option<(u32, u32)>,
    builds: u64,
}

fn storage_texture(binding: u32, format: wgpu::TextureFormat) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format,
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        count: None,
    }
}

/// A uniform binding of the shared parameter block.
pub(crate) fn params_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: wgpu::BufferSize::new(size_of::<DebugParamsGpu>() as u64),
        },
        count: None,
    }
}

impl DepthGBuffer {
    /// Compiles the stage (one compute pipeline).
    #[must_use]
    pub fn new(device: &TrackedDevice) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("restir di depth gbuffer"),
            source: crate::wgsl_source(SOURCE),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("restir di gbuffer bgl"),
            entries: &[
                params_entry(0, wgpu::ShaderStages::COMPUTE),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                storage_texture(2, DEPTH_GBUFFER_FORMATS[0]),
                storage_texture(3, DEPTH_GBUFFER_FORMATS[1]),
                storage_texture(4, DEPTH_GBUFFER_FORMATS[2]),
                storage_texture(5, DEPTH_GBUFFER_FORMATS[3]),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("restir di gbuffer"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("restir di gbuffer"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("gbuffer_from_depth"),
            compilation_options: Default::default(),
            cache: crate::pipeline_cache::cache_for(device).as_deref(),
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("restir di gbuffer params"),
            size: size_of::<DebugParamsGpu>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            layout,
            pipeline,
            params,
            params_bytes: DebugParamsGpu::default(),
            targets: None,
            prev_clip_from_world: None,
            pending: None,
            builds: 0,
        }
    }

    /// Forgets the previous view-projection: the next frame's motion is zero (a camera cut).
    pub fn reset_motion(&mut self) {
        self.prev_clip_from_world = None;
    }

    /// Times the outputs were (re)built.
    #[must_use]
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// The outputs (linear depth, normal + roughness, albedo + metallic, motion) after the first
    /// [`Self::prepare`].
    #[must_use]
    pub fn views(&self) -> Option<&[wgpu::TextureView; 4]> {
        self.targets.as_ref().map(|t| &t.views)
    }

    /// Their size.
    #[must_use]
    pub fn size(&self) -> Option<(u32, u32)> {
        self.targets.as_ref().map(|t| t.size)
    }

    /// The parameter block (bind it in a composite that reads the same viewport and pre-exposure).
    #[must_use]
    pub fn params_buffer(&self) -> &wgpu::Buffer {
        &self.params
    }

    /// Sizes the outputs to the viewport, binds the depth and fills the parameter block.
    /// Allocates nothing once the size and the depth view are unchanged.
    ///
    /// # Errors
    /// [`RestirDiError::BadInput`] for an empty viewport.
    pub fn prepare(
        &mut self,
        device: &TrackedDevice,
        inputs: &DepthGBufferInputs<'_>,
    ) -> Result<(), RestirDiError> {
        let size = (
            inputs.viewport[2].round().max(0.0) as u32,
            inputs.viewport[3].round().max(0.0) as u32,
        );
        if size.0 == 0 || size.1 == 0 {
            return Err(RestirDiError::BadInput("the viewport is empty".into()));
        }
        if self.targets.as_ref().is_none_or(|t| t.size != size) {
            self.targets = Some(build_targets(device, size));
            self.builds += 1;
        }
        let Some(targets) = self.targets.as_mut() else {
            return Err(RestirDiError::BadInput("outputs missing".into()));
        };
        if targets.group.as_ref().is_none_or(|(v, _)| v != inputs.depth) {
            let view = |i: usize| wgpu::BindingResource::TextureView(&targets.views[i]);
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("restir di gbuffer"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(inputs.depth),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: view(0),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: view(1),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: view(2),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: view(3),
                    },
                ],
            });
            targets.group = Some((inputs.depth.clone(), group));
        }
        let forward = Vec3::from(inputs.forward).normalize_or(Vec3::NEG_Z);
        let prev = self.prev_clip_from_world;
        self.params_bytes = DebugParamsGpu {
            world_from_clip: inputs.clip_from_world.inverse().to_cols_array_2d(),
            prev_clip_from_world: prev.unwrap_or(inputs.clip_from_world).to_cols_array_2d(),
            eye: [inputs.eye[0], inputs.eye[1], inputs.eye[2], inputs.pre_exposure],
            forward: [
                forward.x,
                forward.y,
                forward.z,
                if prev.is_some() { 1.0 } else { 0.0 },
            ],
            viewport: inputs.viewport,
            sizes: [
                inputs.color_size.0 as f32,
                inputs.color_size.1 as f32,
                size.0 as f32,
                size.1 as f32,
            ],
            material: inputs.material,
        };
        self.prev_clip_from_world = Some(inputs.clip_from_world);
        self.pending = Some(size);
        Ok(())
    }

    /// The parameter block [`Self::prepare`] filled: write `bytes` into `buffer` at offset 0
    /// through the caller's queue before the encoded pass runs.
    #[must_use]
    pub fn params_upload(&self) -> (&wgpu::Buffer, &[u8]) {
        (&self.params, bytemuck::bytes_of(&self.params_bytes))
    }

    /// Records the prepared frame's pass (nothing when none is prepared).
    pub fn encode(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let Some(size) = self.pending.take() else {
            return;
        };
        let Some((_, group)) = self.targets.as_ref().and_then(|t| t.group.as_ref()) else {
            return;
        };
        let _span = tracing::trace_span!("restir_di_depth_gbuffer").entered();
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("restir di gbuffer"),
            timestamp_writes: None,
        });
        pass.counted_set_pipeline(&self.pipeline);
        pass.counted_set_bind_group(0, group, &[]);
        pass.counted_dispatch(size.0.div_ceil(8), size.1.div_ceil(8), 1);
    }
}

fn build_targets(device: &TrackedDevice, size: (u32, u32)) -> Targets {
    let make = |format: wgpu::TextureFormat, label: &str| {
        let t = device.create_texture(&wgpu::TextureDescriptor {
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
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let v = t.create_view(&wgpu::TextureViewDescriptor::default());
        (t, v)
    };
    let (t0, v0) = make(DEPTH_GBUFFER_FORMATS[0], "restir di depth");
    let (t1, v1) = make(DEPTH_GBUFFER_FORMATS[1], "restir di normal");
    let (t2, v2) = make(DEPTH_GBUFFER_FORMATS[2], "restir di albedo");
    let (t3, v3) = make(DEPTH_GBUFFER_FORMATS[3], "restir di motion");
    Targets {
        size,
        views: [v0, v1, v2, v3],
        _textures: [t0, t1, t2, t3],
        group: None,
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "restir_di_composite.wgsl,restir_di_debug.wgsl",
        "DebugParams",
        DebugParamsGpu {
            world_from_clip,
            prev_clip_from_world,
            eye,
            forward,
            viewport,
            sizes,
            material
        }
    )]
}
