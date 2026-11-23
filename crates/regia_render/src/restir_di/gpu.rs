// SPDX-License-Identifier: MPL-2.0
//! The GPU twin of [`super::cpu::ReferenceSampler`] (`shaders/restir_di.wgsl`, ADR-2470).
//!
//! [`RestirDiGpu`] records three compute passes from explicit inputs ([`RestirInputs`]) into a
//! caller's encoder and never submits. Its reservoirs, surface guides and output live on the
//! GPU and are sized to the inputs; after the first frame at a size nothing is created again
//! (no buffer, texture, bind group or pipeline) while the inputs stay the same objects.
//!
//! A frame is [`RestirDiGpu::prepare`] (validates, sizes, binds, fills the parameter block),
//! the caller's upload of [`RestirDiGpu::params_upload`] through its own queue, then
//! [`RestirDiGpu::encode`]. [`RestirDiGpu::record`] does all three with a plain queue.

use crate::frame_work::counted::*;
use regia_core::glam::Vec3;

use super::{MAX_SPATIAL_TAPS, RestirCamera, RestirDiError, RestirDiSettings};
use crate::TrackedDevice;

const SOURCE: &str = include_str!("../../shaders/restir_di.wgsl");
const WORKGROUP: u32 = 8;
const FLAG_HISTORY: u32 = 1;
const FLAG_CLUSTERS: u32 = 2;
const FLAG_TRACE: u32 = 4;
/// Bytes per pixel of a reservoir (`Reservoir` in the shader).
const RESERVOIR_BYTES: u64 = 16;
/// Bytes per pixel of a surface guide (two `vec4<f32>`).
const GUIDE_BYTES: u64 = 32;
/// The output's format.
pub(crate) const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

const _: () = assert!(MAX_SPATIAL_TAPS + 1 == 9, "`accepted` in restir_di.wgsl holds 9");

/// The frame's clustered light lists (`cluster_build.wgsl`'s `counts` and `indices`).
#[derive(Clone, Copy, Debug)]
pub struct RestirClusters<'a> {
    /// Lights per cluster.
    pub counts: &'a wgpu::Buffer,
    /// `cluster × max_per_cluster + slot` → light index.
    pub indices: &'a wgpu::Buffer,
    /// Slots per cluster.
    pub max_per_cluster: u32,
}

/// One frame's inputs, all at `size` (except the buffers).
#[derive(Clone, Copy, Debug)]
pub struct RestirInputs<'a> {
    /// Width and height in pixels.
    pub size: (u32, u32),
    /// Linear view depth in `r` (metres; 0 = no surface). Any float format.
    pub depth: &'a wgpu::TextureView,
    /// World normal in `xyz`, perceptual roughness in `w`.
    pub normal_roughness: &'a wgpu::TextureView,
    /// Base color in `rgb`, metallic in `a`.
    pub albedo_metallic: &'a wgpu::TextureView,
    /// `prev_uv - uv` in `xy` (the frame's velocity convention).
    pub motion: &'a wgpu::TextureView,
    /// Local lights in the frame's record layout ([`super::ManyLight`]).
    pub lights: &'a wgpu::Buffer,
    /// Records in `lights`.
    pub light_count: u32,
    /// Candidates come from the pixel's cluster list; `None` draws from every light.
    pub clusters: Option<RestirClusters<'a>>,
    /// The camera of `depth`.
    pub camera: RestirCamera,
    /// Write the brute-force sum over the pixel's list instead of the estimate (evaluation).
    pub reference: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct ParamsGpu {
    size: [u32; 4],
    cfg: [u32; 4],
    tol: [f32; 4],
    eye: [f32; 4],
    forward: [f32; 4],
    planes: [f32; 4],
    extra: [u32; 4],
    world_from_clip: [[f32; 4]; 4],
    clip_from_world: [[f32; 4]; 4],
}

/// The objects a bind group references (compared by identity).
#[derive(Clone, PartialEq)]
struct BindKey {
    generation: u64,
    views: [wgpu::TextureView; 4],
    lights: wgpu::Buffer,
    counts: wgpu::Buffer,
    indices: wgpu::Buffer,
}

struct Resources {
    size: (u32, u32),
    guides: [wgpu::Buffer; 2],
    reservoirs: [wgpu::Buffer; 2],
    tmp: wgpu::Buffer,
    output: wgpu::Texture,
    output_view: wgpu::TextureView,
    /// Per parity: the inputs it binds and the group.
    groups: [Option<(BindKey, wgpu::BindGroup)>; 2],
}

/// The GPU many-light sampler: pipelines, per-size reservoirs and history.
pub struct RestirDiGpu {
    settings: RestirDiSettings,
    seed: u32,
    layout: wgpu::BindGroupLayout,
    initial_temporal: wgpu::ComputePipeline,
    spatial: wgpu::ComputePipeline,
    shade: wgpu::ComputePipeline,
    reference: wgpu::ComputePipeline,
    params: wgpu::Buffer,
    params_bytes: ParamsGpu,
    empty: wgpu::Buffer,
    resources: Option<Resources>,
    frame: u32,
    has_history: bool,
    last_light_count: u32,
    pending: Option<(bool, (u32, u32))>,
    resource_builds: u64,
    bind_group_builds: u64,
}

impl std::fmt::Debug for RestirDiGpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestirDiGpu")
            .field("settings", &self.settings)
            .field("size", &self.resources.as_ref().map(|r| r.size))
            .field("frame", &self.frame)
            .field("has_history", &self.has_history)
            .finish_non_exhaustive()
    }
}

fn storage(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn texture(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

impl RestirDiGpu {
    /// Compiles the passes on `device` (four compute pipelines, counted by the tracked device).
    ///
    /// # Errors
    /// [`RestirDiError::InvalidSetting`] when `settings` fails validation.
    pub fn new(device: &TrackedDevice, settings: RestirDiSettings, seed: u32) -> Result<Self, RestirDiError> {
        settings.validate()?;
        let _span = tracing::debug_span!("restir_di_compile").entered();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("restir di"),
            source: crate::wgsl_source(SOURCE),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("restir di bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(size_of::<ParamsGpu>() as u64),
                    },
                    count: None,
                },
                texture(1),
                texture(2),
                texture(3),
                texture(4),
                storage(5, true),
                storage(6, true),
                storage(7, true),
                storage(8, false),
                storage(9, true),
                storage(10, false),
                storage(11, false),
                storage(12, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 13,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: OUTPUT_FORMAT,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("restir di"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipe = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: crate::pipeline_cache::cache_for(device).as_deref(),
            })
        };
        let initial_temporal = pipe("initial_temporal");
        let spatial = pipe("spatial");
        let shade = pipe("shade_pass");
        let reference = pipe("reference_pass");
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("restir di params"),
            size: size_of::<ParamsGpu>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let empty = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("restir di empty lists"),
            size: 16,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        Ok(Self {
            settings,
            seed,
            layout,
            initial_temporal,
            spatial,
            shade,
            reference,
            params,
            params_bytes: ParamsGpu::default(),
            empty,
            resources: None,
            frame: 0,
            has_history: false,
            last_light_count: 0,
            pending: None,
            resource_builds: 0,
            bind_group_builds: 0,
        })
    }

    /// The settings in use.
    #[must_use]
    pub fn settings(&self) -> &RestirDiSettings {
        &self.settings
    }

    /// Replaces the settings and drops the history.
    ///
    /// # Errors
    /// [`RestirDiError::InvalidSetting`]; the old settings stay.
    pub fn set_settings(&mut self, settings: RestirDiSettings) -> Result<(), RestirDiError> {
        settings.validate()?;
        self.settings = settings;
        self.has_history = false;
        Ok(())
    }

    /// Drops the history (a camera cut, or a light list whose order changed).
    pub fn reset_history(&mut self) {
        self.has_history = false;
    }

    /// Frames encoded.
    #[must_use]
    pub fn frames(&self) -> u32 {
        self.frame
    }

    /// Whether the next frame reuses the previous one's reservoirs.
    #[must_use]
    pub fn has_history(&self) -> bool {
        self.has_history
    }

    /// Drops both parity groups when the frame replaces its shared cluster indices.
    pub(crate) fn invalidate_bindings(&mut self) {
        if let Some(resources) = self.resources.as_mut() {
            resources.groups = [None, None];
        }
    }

    /// Current/history reservoir roles and temporal scratch allocation, plus resource generation.
    pub(crate) fn graph_buffers(&self) -> ([u64; 3], u64) {
        let sizes = self.resources.as_ref().map_or([0; 3], |r| {
            [r.reservoirs[0].size(), r.reservoirs[1].size(), r.tmp.size()]
        });
        (sizes, self.resource_builds)
    }

    /// Times the per-size resources were (re)built.
    #[must_use]
    pub fn resource_builds(&self) -> u64 {
        self.resource_builds
    }

    /// Bind groups built.
    #[must_use]
    pub fn bind_group_builds(&self) -> u64 {
        self.bind_group_builds
    }

    /// The output image (`Rgba16Float`, linear radiance of the local lights; `a` = 1 on
    /// surfaces), after the first [`Self::prepare`].
    #[must_use]
    pub fn output_view(&self) -> Option<&wgpu::TextureView> {
        self.resources.as_ref().map(|r| &r.output_view)
    }

    /// The reservoir buffer the last encoded frame wrote, one per pixel row-major
    /// (`light: u32` (`u32::MAX` = none), `w: f32`, `m: f32`, `pad: f32`): what the forward pass
    /// reads in the ADR-2470 hookup to shade the pixel's light. `None` before the first frame.
    #[must_use]
    pub fn reservoirs(&self) -> Option<&wgpu::Buffer> {
        if self.frame == 0 {
            return None;
        }
        let last = (self.frame.wrapping_sub(1) & 1) as usize;
        self.resources.as_ref().map(|r| &r.reservoirs[last])
    }

    /// Sizes the per-size resources to `size` ahead of [`Self::prepare`] (nothing when they
    /// already are), so a caller can bind [`Self::next_reservoirs`] before the frame's inputs
    /// exist (the frame hookup binds it in its view group before the depth prepass, ADR-2652).
    /// A zero size does nothing.
    pub fn ensure_size(&mut self, device: &TrackedDevice, size: (u32, u32)) {
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        if self.resources.as_ref().is_none_or(|r| r.size != size) {
            self.build(device, size);
        }
    }

    /// The reservoir buffer the next [`Self::encode`] writes (the shade pass reads it, and the
    /// forward pass of the ADR-2652 hookup after it), once the resources are sized; it
    /// alternates between two buffers by frame parity. [`Self::reservoirs`] names the same
    /// buffer after that encode.
    #[must_use]
    pub fn next_reservoirs(&self) -> Option<&wgpu::Buffer> {
        let cur = (self.frame & 1) as usize;
        self.resources.as_ref().map(|r| &r.reservoirs[cur])
    }

    /// The parity of the next encoded frame (0 or 1): which of the two reservoir buffers
    /// [`Self::next_reservoirs`] is.
    #[must_use]
    pub fn next_parity(&self) -> u32 {
        self.frame & 1
    }

    /// The output texture (for a copy).
    #[must_use]
    pub fn output_texture(&self) -> Option<&wgpu::Texture> {
        self.resources.as_ref().map(|r| &r.output)
    }

    /// Bytes of the per-size resources (reservoirs, guides, output) at `size`.
    #[must_use]
    pub fn bytes_at(size: (u32, u32)) -> u64 {
        let px = u64::from(size.0) * u64::from(size.1);
        px * (3 * RESERVOIR_BYTES + 2 * GUIDE_BYTES + 8)
    }

    fn build(&mut self, device: &TrackedDevice, size: (u32, u32)) {
        let px = u64::from(size.0) * u64::from(size.1);
        let buffer = |label: &str, bytes: u64| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: bytes.max(16),
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            })
        };
        let output = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("restir di output"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OUTPUT_FORMAT,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let output_view = output.create_view(&wgpu::TextureViewDescriptor::default());
        self.resources = Some(Resources {
            size,
            guides: [
                buffer("restir di guide a", px * GUIDE_BYTES),
                buffer("restir di guide b", px * GUIDE_BYTES),
            ],
            reservoirs: [
                buffer("restir di reservoirs a", px * RESERVOIR_BYTES),
                buffer("restir di reservoirs b", px * RESERVOIR_BYTES),
            ],
            tmp: buffer("restir di reservoirs tmp", px * RESERVOIR_BYTES),
            output,
            output_view,
            groups: [None, None],
        });
        self.resource_builds += 1;
        self.has_history = false;
    }

    /// Validates the inputs, (re)builds what their size needs, binds them and fills the
    /// parameter block. Allocates nothing once the size and the input objects are unchanged.
    ///
    /// # Errors
    /// [`RestirDiError::BadInput`] for a zero size, a light count past the buffer, or cluster
    /// slots of 0.
    pub fn prepare(
        &mut self,
        device: &TrackedDevice,
        inputs: &RestirInputs<'_>,
    ) -> Result<(), RestirDiError> {
        let (w, h) = inputs.size;
        if w == 0 || h == 0 {
            return Err(RestirDiError::BadInput("the size must not be zero".into()));
        }
        if u64::from(inputs.light_count) * super::MANY_LIGHT_BYTES as u64 > inputs.lights.size() {
            return Err(RestirDiError::BadInput(format!(
                "{} lights do not fit a {}-byte buffer",
                inputs.light_count,
                inputs.lights.size()
            )));
        }
        if let Some(c) = inputs.clusters
            && c.max_per_cluster == 0
        {
            return Err(RestirDiError::BadInput("max_per_cluster must not be zero".into()));
        }
        if self.resources.as_ref().is_none_or(|r| r.size != inputs.size) {
            self.build(device, inputs.size);
        }
        if inputs.light_count != self.last_light_count {
            self.has_history = false;
            self.last_light_count = inputs.light_count;
        }
        let parity = (self.frame & 1) as usize;
        let key = BindKey {
            generation: self.resource_builds,
            views: [
                inputs.depth.clone(),
                inputs.normal_roughness.clone(),
                inputs.albedo_metallic.clone(),
                inputs.motion.clone(),
            ],
            lights: inputs.lights.clone(),
            counts: inputs
                .clusters
                .map_or_else(|| self.empty.clone(), |c| c.counts.clone()),
            indices: inputs
                .clusters
                .map_or_else(|| self.empty.clone(), |c| c.indices.clone()),
        };
        let Some(res) = self.resources.as_mut() else {
            return Err(RestirDiError::BadInput("resources missing".into()));
        };
        // A replaced shared input must not remain retained by the inactive parity.
        if res.groups.iter().flatten().any(|(k, _)| {
            k.generation != key.generation
                || k.lights != key.lights
                || k.counts != key.counts
                || k.indices != key.indices
        }) {
            res.groups = [None, None];
        }
        if res.groups[parity].as_ref().is_none_or(|(k, _)| *k != key) {
            let (cur, prev) = (parity, 1 - parity);
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("restir di"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&key.views[0]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&key.views[1]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(&key.views[2]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::TextureView(&key.views[3]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: key.lights.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 6,
                        resource: key.counts.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 7,
                        resource: key.indices.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 8,
                        resource: res.guides[cur].as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 9,
                        resource: res.guides[prev].as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 10,
                        resource: res.tmp.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 11,
                        resource: res.reservoirs[cur].as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 12,
                        resource: res.reservoirs[prev].as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 13,
                        resource: wgpu::BindingResource::TextureView(&res.output_view),
                    },
                ],
            });
            res.groups[parity] = Some((key, group));
            self.bind_group_builds += 1;
        }
        let s = &self.settings;
        let history = self.has_history && s.temporal;
        let mut flags = 0;
        if history {
            flags |= FLAG_HISTORY;
        }
        if inputs.clusters.is_some() {
            flags |= FLAG_CLUSTERS;
        }
        if s.trace_steps > 0 {
            flags |= FLAG_TRACE;
        }
        let cam = &inputs.camera;
        let forward = Vec3::from(cam.forward).normalize_or(Vec3::NEG_Z);
        self.params_bytes = ParamsGpu {
            size: [w, h, inputs.light_count, self.frame],
            cfg: [
                s.initial_candidates,
                s.spatial_taps,
                flags,
                inputs.clusters.map_or(0, |c| c.max_per_cluster),
            ],
            tol: [
                s.depth_tolerance,
                s.normal_tolerance,
                s.history_cap,
                s.spatial_radius_px,
            ],
            eye: [cam.eye[0], cam.eye[1], cam.eye[2], s.trace_thickness],
            forward: [forward.x, forward.y, forward.z, 0.0],
            planes: [cam.near, cam.far, 0.0, 0.0],
            extra: [self.seed, s.trace_steps, 0, 0],
            world_from_clip: cam.clip_from_world.inverse().to_cols_array_2d(),
            clip_from_world: cam.clip_from_world.to_cols_array_2d(),
        };
        self.pending = Some((inputs.reference, inputs.size));
        Ok(())
    }

    /// The parameter block [`Self::prepare`] filled: write `bytes` into `buffer` at offset 0
    /// through the caller's queue before the encoded passes run.
    #[must_use]
    pub fn params_upload(&self) -> (&wgpu::Buffer, &[u8]) {
        (&self.params, bytemuck::bytes_of(&self.params_bytes))
    }

    /// Records the passes of the prepared frame into `encoder` (nothing when no frame is
    /// prepared) and advances the history.
    pub fn encode(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let Some((reference, (w, h))) = self.pending.take() else {
            return;
        };
        let parity = (self.frame & 1) as usize;
        let Some(group) = self
            .resources
            .as_ref()
            .and_then(|r| r.groups[parity].as_ref())
            .map(|(_, g)| g)
        else {
            return;
        };
        let _span = tracing::trace_span!("restir_di").entered();
        let groups = (w.div_ceil(WORKGROUP), h.div_ceil(WORKGROUP));
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("restir di"),
            timestamp_writes: None,
        });
        pass.counted_set_bind_group(0, group, &[]);
        let last = if reference { &self.reference } else { &self.shade };
        for pipeline in [&self.initial_temporal, &self.spatial, last] {
            pass.counted_set_pipeline(pipeline);
            pass.counted_dispatch(groups.0, groups.1, 1);
        }
        drop(pass);
        self.frame = self.frame.wrapping_add(1);
        self.has_history = true;
    }

    /// [`Self::prepare`], the parameter upload on `queue`, and [`Self::encode`].
    ///
    /// # Errors
    /// See [`Self::prepare`].
    pub fn record(
        &mut self,
        device: &TrackedDevice,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &RestirInputs<'_>,
    ) -> Result<(), RestirDiError> {
        self.prepare(device, inputs)?;
        let (buffer, bytes) = self.params_upload();
        queue.write_buffer(buffer, 0, bytes);
        self.encode(encoder);
        Ok(())
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "restir_di.wgsl",
        "Params",
        ParamsGpu {
            size,
            cfg,
            tol,
            eye,
            forward,
            planes,
            extra,
            world_from_clip,
            clip_from_world
        }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_buffers_di_resize_and_cluster_replacement_drop_both_parities() {
        let device = crate::RenderDevice::headless().expect("lavapipe device required");
        let d = device.tracked();
        let mut sampler = RestirDiGpu::new(&d, RestirDiSettings::default(), 7).expect("buffer audit");
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("buffer audit guides"),
            size: wgpu::Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let storage = || {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("buffer audit lights/clusters"),
                size: 256,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            })
        };
        let lights = storage();
        let counts = storage();
        let old_indices = storage();
        let inputs = RestirInputs {
            size: (4, 4),
            depth: &view,
            normal_roughness: &view,
            albedo_metallic: &view,
            motion: &view,
            lights: &lights,
            light_count: 0,
            clusters: Some(RestirClusters {
                counts: &counts,
                indices: &old_indices,
                max_per_cluster: 1,
            }),
            camera: RestirCamera::look_at([0.0, 0.0, -2.0], [0.0; 3], 1.0, 1.0, 0.1, 10.0),
            reference: false,
        };
        // Preparing both parities is enough to build their groups; no shader needs to run.
        for frame in 0..2 {
            sampler.frame = frame;
            sampler.prepare(&d, &inputs).expect("buffer audit");
        }
        assert!(
            sampler
                .resources
                .as_ref()
                .expect("buffer audit")
                .groups
                .iter()
                .all(Option::is_some)
        );
        let new_indices = storage();
        sampler
            .prepare(
                &d,
                &RestirInputs {
                    clusters: Some(RestirClusters {
                        counts: &counts,
                        indices: &new_indices,
                        max_per_cluster: 1,
                    }),
                    ..inputs
                },
            )
            .expect("buffer audit");
        let resources = sampler.resources.as_ref().expect("buffer audit");
        assert!(
            resources
                .groups
                .iter()
                .flatten()
                .all(|(key, _)| key.indices == new_indices)
        );
        assert_eq!(
            resources.groups.iter().flatten().count(),
            1,
            "inactive parity must release its retired input"
        );
        let old_pair = resources.reservoirs.clone();
        let generation = sampler.resource_builds();
        sampler.ensure_size(&d, (8, 4));
        let resources = sampler.resources.as_ref().expect("buffer audit");
        assert!(
            resources.groups.iter().all(Option::is_none),
            "no group over old reservoirs survives"
        );
        assert!(
            resources
                .reservoirs
                .iter()
                .all(|buffer| !old_pair.contains(buffer))
        );
        assert_eq!(sampler.resource_builds(), generation + 1);
        assert_eq!(sampler.graph_buffers().0, [8 * 4 * RESERVOIR_BYTES; 3]);
        sampler
            .prepare(
                &d,
                &RestirInputs {
                    size: (8, 4),
                    ..inputs
                },
            )
            .expect("buffer audit");
        assert!(
            sampler
                .resources
                .as_ref()
                .expect("buffer audit")
                .groups
                .iter()
                .flatten()
                .all(|(key, _)| key.generation == sampler.resource_builds())
        );
    }

    #[test]
    fn the_shader_parses_and_validates() {
        let module = match naga::front::wgsl::parse_str(SOURCE) {
            Ok(m) => m,
            Err(e) => panic!("{}", e.emit_to_string(SOURCE)),
        };
        let mut v = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::default(),
        );
        if let Err(e) = v.validate(&module) {
            panic!("{}", e.emit_to_string(SOURCE));
        }
        for entry in ["initial_temporal", "spatial", "shade_pass", "reference_pass"] {
            assert!(module.entry_points.iter().any(|e| e.name == entry), "{entry}");
        }
    }

    #[test]
    fn the_parameter_block_matches_the_shader() {
        // 7 vec4 rows and two mat4x4: the WGSL struct's size with its 16-byte alignment.
        assert_eq!(size_of::<ParamsGpu>(), 7 * 16 + 2 * 64);
    }
}
