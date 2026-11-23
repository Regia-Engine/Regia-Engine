// SPDX-License-Identifier: MPL-2.0
//! Persistent GPU resources for ReSTIR GI. `prepare` sizes and binds, the caller uploads both
//! parameter blocks, and `encode` records into the frame's encoder without submitting or polling.
//! After the first frame at a size and with unchanged input objects, no GPU object is created.

use std::sync::{Arc, atomic::AtomicU8, atomic::Ordering};

use regia_core::glam::Mat4;

use super::{RestirGiError, RestirGiSettings, RestirGiWork};
use crate::TrackedDevice;
use crate::frame_work::counted::*;

/// Full module: ray-query extension declarations must precede every other declaration.
pub(crate) const SOURCE: &str = concat!(
    "enable wgpu_ray_query;\n",
    "enable wgpu_ray_query_vertex_return;\n",
    include_str!("../../shaders/ddgi_common.wgsl"),
    "\n",
    include_str!("../../shaders/ddgi_sample.wgsl"),
    "\n",
    include_str!("../../shaders/gi_denoise.wgsl"),
    "\n",
    include_str!("restir_gi.wgsl"),
);

const WORKGROUP: u32 = 8;
const VEC4: u64 = 16;
const RESERVOIR_BYTES: u64 = 96;
const INPUT_SLOTS: u64 = 3;
const HISTORY_SLOTS: u64 = 3;
const ACC_SLOTS: u64 = 2;
const PARAM_SLOTS: u64 = 1 + crate::gi_denoise::MAX_ATROUS_ITERATIONS as u64;
const STATS_BYTES: u64 = 13 * 4;
const FLAG_HISTORY: u32 = 1;
const FLAG_TEMPORAL: u32 = 2;
const FLAG_VALIDATE: u32 = 4;
const FLAG_STORE_FIRST_PASS: u32 = 2;
const MAP_PENDING: u8 = 0;
const MAP_DONE: u8 = 1;
const MAP_FAILED: u8 = 2;

const _: () = assert!(RESERVOIR_BYTES == 6 * VEC4);

/// One frame's existing renderer resources.
#[derive(Clone, Copy)]
pub struct RestirGiInputs<'a> {
    /// Internal render width and height.
    pub size: (u32, u32),
    /// DDGI screen-lookup width and height (the composite's downsample target).
    pub screen_size: (u32, u32),
    /// Internal pixels per DDGI screen texel (1, 2 or 4).
    pub screen_step: u32,
    /// Linear view depth (`r`, 0 = sky).
    pub depth: &'a wgpu::TextureView,
    /// World normal in `xyz`.
    pub normal_roughness: &'a wgpu::TextureView,
    /// Previous UV minus current UV in `xy`.
    pub motion: &'a wgpu::TextureView,
    /// Single-sampled scene depth (standard 0..1, 1 = sky).
    pub scene_depth: &'a wgpu::TextureView,
    /// Inverse of the view-projection that drew `scene_depth`.
    pub world_from_clip: Mat4,
    /// Top-level structure built from the frame's casters.
    pub tlas: &'a wgpu::Tlas,
    /// Changes whenever `tlas` is recreated; keys the bind group.
    pub scene_epoch: u64,
    /// Existing probe uniform.
    pub ddgi_uniform: &'a wgpu::Buffer,
    /// Existing probe data, including the screen region this pass replaces.
    pub ddgi_data: &'a wgpu::Buffer,
    /// `HitMaterial` records indexed by each opaque instance's `custom_data - 1`.
    pub materials: &'a wgpu::Buffer,
    /// Records in `materials`.
    pub material_count: u32,
    /// Camera position.
    pub eye: [f32; 3],
    /// Longest path and visibility ray, world units.
    pub far: f32,
}

impl std::fmt::Debug for RestirGiInputs<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestirGiInputs")
            .field("size", &self.size)
            .field("scene_epoch", &self.scene_epoch)
            .field("material_count", &self.material_count)
            .field("eye", &self.eye)
            .field("far", &self.far)
            .finish_non_exhaustive()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct ParamsGpu {
    world_from_clip: [[f32; 4]; 4],
    size: [u32; 4],
    cfg: [u32; 4],
    tol: [f32; 4],
    ray: [f32; 4],
    validation: [f32; 4],
    eye: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct DenoiseParamsGpu {
    size: [u32; 4],
    temporal: [f32; 4],
    edge: [f32; 4],
    extra: [f32; 4],
    antilag: [f32; 4],
}

struct Pipelines {
    restir_layout: wgpu::BindGroupLayout,
    composite_layout: wgpu::BindGroupLayout,
    initial_temporal: wgpu::ComputePipeline,
    spatial: wgpu::ComputePipeline,
    resolve: wgpu::ComputePipeline,
    composite: wgpu::ComputePipeline,
    denoise_layout: wgpu::BindGroupLayout,
    reproject: wgpu::ComputePipeline,
    antilag: wgpu::ComputePipeline,
    temporal: wgpu::ComputePipeline,
    variance: wgpu::ComputePipeline,
    atrous: wgpu::ComputePipeline,
    store: wgpu::ComputePipeline,
}

#[derive(Clone, PartialEq)]
struct BindKey {
    generation: u64,
    depth: wgpu::TextureView,
    normal: wgpu::TextureView,
    motion: wgpu::TextureView,
    scene_depth: wgpu::TextureView,
    ddgi_uniform: wgpu::Buffer,
    ddgi_data: wgpu::Buffer,
    materials: wgpu::Buffer,
    scene_epoch: u64,
}

struct RestirGroup {
    key: BindKey,
    group: wgpu::BindGroup,
    composite: wgpu::BindGroup,
}

enum StatsReadback {
    Idle,
    Copied,
    Mapping(Arc<AtomicU8>),
}

struct Pending {
    parity: usize,
    size: (u32, u32),
    screen_size: (u32, u32),
}

struct Resources {
    size: (u32, u32),
    reservoirs: [wgpu::Buffer; 2],
    temporal: wgpu::Buffer,
    frame_input: wgpu::Buffer,
    _denoise_history: wgpu::Buffer,
    _acc: wgpu::Buffer,
    _blocks: wgpu::Buffer,
    _ping: wgpu::Buffer,
    _pong: wgpu::Buffer,
    stats: wgpu::Buffer,
    stats_staging: wgpu::Buffer,
    stats_state: StatsReadback,
    groups: [Option<RestirGroup>; 2],
    to_ping: wgpu::BindGroup,
    to_pong: wgpu::BindGroup,
}

/// Persistent ReSTIR GI pipelines, reservoirs, denoiser history and asynchronous work readback.
pub struct RestirGiGpu {
    settings: RestirGiSettings,
    seed: u32,
    pipes: Pipelines,
    null_group: wgpu::BindGroup,
    _null_read: wgpu::Buffer,
    _null_write: wgpu::Buffer,
    params: wgpu::Buffer,
    params_bytes: ParamsGpu,
    denoise_params: wgpu::Buffer,
    denoise_stride: u64,
    denoise_bytes: Vec<u8>,
    resources: Option<Resources>,
    pending: Option<Pending>,
    frame: u32,
    has_history: bool,
    work: Option<RestirGiWork>,
    total_work: RestirGiWork,
    resource_builds: u64,
    bind_group_builds: u64,
}

impl std::fmt::Debug for RestirGiGpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestirGiGpu")
            .field("settings", &self.settings)
            .field("size", &self.resources.as_ref().map(|r| r.size))
            .field("frame", &self.frame)
            .field("has_history", &self.has_history)
            .field("work", &self.work)
            .finish_non_exhaustive()
    }
}

fn buffer_entry(binding: u32, ty: wgpu::BufferBindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn storage(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    buffer_entry(binding, wgpu::BufferBindingType::Storage { read_only })
}

fn texture(binding: u32, depth: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Texture {
            sample_type: if depth {
                wgpu::TextureSampleType::Depth
            } else {
                wgpu::TextureSampleType::Float { filterable: false }
            },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn buf(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

fn view(binding: u32, texture: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::TextureView(texture),
    }
}

fn denoise_layout(device: &TrackedDevice) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("restir gi denoise bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(size_of::<DenoiseParamsGpu>() as u64),
                },
                count: None,
            },
            storage(1, true),
            storage(2, false),
            storage(3, false),
            storage(4, true),
            storage(5, false),
            storage(6, false),
        ],
    })
}

impl RestirGiGpu {
    /// Compiles every pass on a ray-query, hit-vertex-return device.
    ///
    /// # Errors
    /// [`RestirGiError::InvalidSetting`] or [`RestirGiError::Unsupported`].
    pub fn new(device: &TrackedDevice, settings: RestirGiSettings, seed: u32) -> Result<Self, RestirGiError> {
        settings.validate()?;
        let features = device.features();
        if !features.contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY) {
            return Err(RestirGiError::Unsupported("EXPERIMENTAL_RAY_QUERY"));
        }
        if !features.contains(wgpu::Features::EXPERIMENTAL_RAY_HIT_VERTEX_RETURN) {
            return Err(RestirGiError::Unsupported("EXPERIMENTAL_RAY_HIT_VERTEX_RETURN"));
        }
        let _span = tracing::debug_span!("restir_gi_compile").entered();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("restir gi"),
            source: crate::wgsl_source(SOURCE),
        });
        let denoise_layout = denoise_layout(device);
        let restir_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("restir gi bgl"),
            entries: &[
                buffer_entry(0, wgpu::BufferBindingType::Uniform),
                texture(1, false),
                texture(2, false),
                texture(3, false),
                texture(4, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::AccelerationStructure { vertex_return: true },
                    count: None,
                },
                buffer_entry(6, wgpu::BufferBindingType::Uniform),
                storage(7, false),
                storage(8, true),
                storage(9, false),
                storage(10, false),
                storage(11, false),
                storage(12, false),
                storage(13, false),
            ],
        });
        let restir_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("restir gi"),
            bind_group_layouts: &[Some(&denoise_layout), Some(&restir_layout)],
            immediate_size: 0,
        });
        let composite_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("restir gi composite bgl"),
            entries: &[
                buffer_entry(0, wgpu::BufferBindingType::Uniform),
                texture(4, true),
                buffer_entry(6, wgpu::BufferBindingType::Uniform),
                storage(7, false),
            ],
        });
        let composite_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("restir gi composite"),
            bind_group_layouts: &[Some(&denoise_layout), Some(&composite_layout)],
            immediate_size: 0,
        });
        let denoise_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("restir gi denoise"),
            bind_group_layouts: &[Some(&denoise_layout)],
            immediate_size: 0,
        });
        let restir_pipe = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&restir_pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: crate::pipeline_cache::cache_for(device).as_deref(),
            })
        };
        let composite = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("gi_composite"),
            layout: Some(&composite_pipeline_layout),
            module: &module,
            entry_point: Some("gi_composite"),
            compilation_options: Default::default(),
            cache: crate::pipeline_cache::cache_for(device).as_deref(),
        });
        let denoise_pipe = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&denoise_pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: crate::pipeline_cache::cache_for(device).as_deref(),
            })
        };
        let pipes = Pipelines {
            restir_layout,
            composite_layout,
            initial_temporal: restir_pipe("gi_initial_temporal"),
            spatial: restir_pipe("gi_spatial"),
            resolve: restir_pipe("gi_resolve"),
            composite,
            denoise_layout,
            reproject: denoise_pipe("reproject"),
            antilag: denoise_pipe("antilag"),
            temporal: denoise_pipe("temporal"),
            variance: denoise_pipe("variance"),
            atrous: denoise_pipe("atrous"),
            store: denoise_pipe("store_history"),
        };
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("restir gi params"),
            size: size_of::<ParamsGpu>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let align = u64::from(device.limits().min_uniform_buffer_offset_alignment).max(1);
        let denoise_stride = (size_of::<DenoiseParamsGpu>() as u64).div_ceil(align) * align;
        let denoise_params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("restir gi denoise params"),
            size: denoise_stride * PARAM_SLOTS,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let null_buffer = |label: &str| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: 16,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            })
        };
        let null_read = null_buffer("restir gi null read");
        let null_write = null_buffer("restir gi null write");
        let null_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("restir gi null denoise group"),
            layout: &pipes.denoise_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &denoise_params,
                        offset: 0,
                        size: wgpu::BufferSize::new(size_of::<DenoiseParamsGpu>() as u64),
                    }),
                },
                buf(1, &null_read),
                buf(2, &null_write),
                buf(3, &null_write),
                buf(4, &null_read),
                buf(5, &null_write),
                buf(6, &null_write),
            ],
        });
        Ok(Self {
            settings,
            seed,
            pipes,
            null_group,
            _null_read: null_read,
            _null_write: null_write,
            params,
            params_bytes: ParamsGpu::default(),
            denoise_params,
            denoise_stride,
            denoise_bytes: vec![0; (denoise_stride * PARAM_SLOTS) as usize],
            resources: None,
            pending: None,
            frame: 0,
            has_history: false,
            work: None,
            total_work: RestirGiWork::default(),
            resource_builds: 0,
            bind_group_builds: 0,
        })
    }

    /// Settings in use.
    #[must_use]
    pub fn settings(&self) -> &RestirGiSettings {
        &self.settings
    }

    /// Replaces settings and drops reservoir and denoiser history.
    ///
    /// # Errors
    /// [`RestirGiError::InvalidSetting`]; the old settings stay.
    pub fn set_settings(&mut self, settings: RestirGiSettings) -> Result<(), RestirGiError> {
        settings.validate()?;
        self.settings = settings;
        self.reset_history();
        Ok(())
    }

    /// Drops reservoir and denoiser history (a camera cut, resize or scene restart).
    pub fn reset_history(&mut self) {
        self.has_history = false;
        self.work = None;
    }

    /// Frames encoded.
    #[must_use]
    pub fn frames(&self) -> u32 {
        self.frame
    }

    /// Whether the next frame can reproject the previous reservoirs and denoiser history.
    #[must_use]
    pub fn has_history(&self) -> bool {
        self.has_history
    }

    /// Latest completed asynchronous counters (`None` before the first readback or after a reset).
    #[must_use]
    pub fn work(&self) -> Option<RestirGiWork> {
        self.work
    }

    /// Sum of every completed readback's counters.
    #[must_use]
    pub fn total_work(&self) -> RestirGiWork {
        self.total_work
    }

    /// Times per-size resources were rebuilt.
    #[must_use]
    pub fn resource_builds(&self) -> u64 {
        self.resource_builds
    }

    /// Current/history reservoir roles and temporal scratch allocation, plus resource generation.
    pub(crate) fn graph_buffers(&self) -> ([u64; 3], u64) {
        let sizes = self.resources.as_ref().map_or([0; 3], |r| {
            [r.reservoirs[0].size(), r.reservoirs[1].size(), r.temporal.size()]
        });
        (sizes, self.resource_builds)
    }

    /// Bind groups built for input/resource changes.
    #[must_use]
    pub fn bind_group_builds(&self) -> u64 {
        self.bind_group_builds
    }

    /// Approximate bytes held at `width × height`, excluding external TLAS/probe/material resources.
    #[must_use]
    pub fn bytes_at(width: u32, height: u32) -> u64 {
        let pixels = u64::from(width) * u64::from(height);
        let reservoirs = 3 * RESERVOIR_BYTES;
        let denoise = (INPUT_SLOTS + HISTORY_SLOTS + ACC_SLOTS + 2) * VEC4;
        pixels * (reservoirs + denoise) + u64::from(width.div_ceil(2)) * u64::from(height.div_ceil(2)) * VEC4
    }

    /// Binds and fills this frame's parameter blocks. Nothing is submitted.
    ///
    /// # Errors
    /// [`RestirGiError::BadInput`] for an empty frame or invalid ray distance.
    pub fn prepare(
        &mut self,
        device: &TrackedDevice,
        inputs: &RestirGiInputs<'_>,
    ) -> Result<(), RestirGiError> {
        let _span = tracing::trace_span!("restir_gi_prepare").entered();
        if inputs.size.0 == 0
            || inputs.size.1 == 0
            || inputs.screen_size.0 == 0
            || inputs.screen_size.1 == 0
            || !matches!(inputs.screen_step, 1 | 2 | 4)
        {
            return Err(RestirGiError::BadInput("the lookup is empty".into()));
        }
        if !(inputs.far.is_finite() && inputs.far > 0.0) {
            return Err(RestirGiError::BadInput(
                "far must be finite and above zero".into(),
            ));
        }
        if Self::ensure_resources(
            device,
            inputs.size,
            &mut self.resources,
            &mut self.resource_builds,
            &self.pipes.denoise_layout,
            &self.denoise_params,
        ) {
            self.has_history = false;
            self.work = None;
        }
        let parity = self.frame as usize & 1;
        let key = BindKey {
            generation: self.resource_builds,
            depth: inputs.depth.clone(),
            normal: inputs.normal_roughness.clone(),
            motion: inputs.motion.clone(),
            scene_depth: inputs.scene_depth.clone(),
            ddgi_uniform: inputs.ddgi_uniform.clone(),
            ddgi_data: inputs.ddgi_data.clone(),
            materials: inputs.materials.clone(),
            scene_epoch: inputs.scene_epoch,
        };
        let resources = self
            .resources
            .as_mut()
            .ok_or_else(|| RestirGiError::BadInput("resources were not created".into()))?;
        if resources.groups[parity].as_ref().is_none_or(|g| g.key != key) {
            let (group, composite) = Self::build_restir_group(
                device,
                &self.pipes.restir_layout,
                &self.pipes.composite_layout,
                &self.params,
                inputs,
                resources,
                parity,
            );
            resources.groups[parity] = Some(RestirGroup {
                key,
                group,
                composite,
            });
            self.bind_group_builds += 2;
        }
        let validate_now = self.settings.validation_interval > 0
            && self.frame.is_multiple_of(self.settings.validation_interval);
        let mut flags = 0;
        if self.has_history {
            flags |= FLAG_HISTORY;
        }
        if self.settings.temporal {
            flags |= FLAG_TEMPORAL;
        }
        if validate_now {
            flags |= FLAG_VALIDATE;
        }
        // TEMPORARY diagnostic: REGIA_GI_DEBUG_SURFACE_LOOKUP=1 makes gi_resolve write the
        // surface-point probe lookup instead of the reservoir estimate.
        self.params_bytes = ParamsGpu {
            world_from_clip: inputs.world_from_clip.to_cols_array_2d(),
            size: [inputs.size.0, inputs.size.1, self.settings.spatial_taps, flags],
            cfg: [
                self.frame,
                self.settings.validation_interval,
                self.seed,
                inputs.material_count,
            ],
            tol: [
                self.settings.history_cap,
                self.settings.spatial_cap,
                self.settings.depth_tolerance,
                self.settings.normal_tolerance,
            ],
            ray: [
                self.settings.ray_offset_m,
                self.settings.ray_offset_per_m,
                inputs.far,
                self.settings.spatial_radius_px,
            ],
            validation: [
                self.settings.validation_luminance_tolerance,
                inputs.screen_step as f32,
                inputs.screen_size.0 as f32,
                inputs.screen_size.1 as f32,
            ],
            eye: [inputs.eye[0], inputs.eye[1], inputs.eye[2], 0.0],
        };
        self.fill_denoise_params(inputs.size);
        self.pending = Some(Pending {
            parity,
            size: inputs.size,
            screen_size: inputs.screen_size,
        });
        Ok(())
    }

    /// ReSTIR parameter buffer and bytes for the caller's queue/staging upload.
    #[must_use]
    pub fn params_upload(&self) -> (&wgpu::Buffer, &[u8]) {
        (&self.params, bytemuck::bytes_of(&self.params_bytes))
    }

    /// Denoiser parameter buffer and all dynamic-offset slots for the caller's upload.
    #[must_use]
    pub fn denoise_params_upload(&self) -> (&wgpu::Buffer, &[u8]) {
        (&self.denoise_params, &self.denoise_bytes)
    }

    /// Records initial/temporal/spatial reuse, denoising and DDGI-screen composition.
    pub fn encode(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let Some(Pending {
            parity,
            size,
            screen_size,
        }) = self.pending.take()
        else {
            return;
        };
        let _span = tracing::info_span!("restir_gi").entered();
        let _crumb = crate::breadcrumbs::frame_pass("restir_gi");
        let Some(resources) = self.resources.as_mut() else {
            return;
        };
        if let Some(work) = step_stats_readback(resources, encoder) {
            self.total_work.accumulate(work);
            self.work = Some(work);
        }
        encoder.clear_buffer(&resources.stats, 0, None);
        let Some((restir_group, composite_group)) = resources.groups[parity]
            .as_ref()
            .map(|g| (g.group.clone(), g.composite.clone()))
        else {
            return;
        };
        let to_ping = resources.to_ping.clone();
        let to_pong = resources.to_pong.clone();
        let groups = (size.0.div_ceil(WORKGROUP), size.1.div_ceil(WORKGROUP));
        let iterations = self.settings.denoise.atrous_iterations;
        let store_first = self.settings.denoise.history_from_first_pass && iterations > 0;
        let slot_offset = |slot: u32| (u64::from(slot) * self.denoise_stride) as u32;
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("restir gi"),
            timestamp_writes: None,
        });
        pass.counted_set_bind_group(0, &self.null_group, &[0]);
        pass.counted_set_bind_group(1, &restir_group, &[]);
        pass.counted_set_pipeline(&self.pipes.initial_temporal);
        pass.counted_dispatch(groups.0, groups.1, 1);
        pass.counted_set_pipeline(&self.pipes.spatial);
        pass.counted_dispatch(groups.0, groups.1, 1);
        pass.counted_set_pipeline(&self.pipes.resolve);
        pass.counted_dispatch(groups.0, groups.1, 1);

        pass.counted_set_bind_group(0, &to_ping, &[slot_offset(0)]);
        pass.counted_set_bind_group(1, &composite_group, &[]);
        pass.counted_set_pipeline(&self.pipes.reproject);
        pass.counted_dispatch(groups.0, groups.1, 1);
        pass.counted_set_pipeline(&self.pipes.antilag);
        let side = self.settings.denoise.antilag_block;
        pass.counted_dispatch(
            size.0.div_ceil(side).div_ceil(WORKGROUP),
            size.1.div_ceil(side).div_ceil(WORKGROUP),
            1,
        );
        pass.counted_set_pipeline(&self.pipes.temporal);
        pass.counted_dispatch(groups.0, groups.1, 1);
        pass.counted_set_pipeline(&self.pipes.variance);
        pass.counted_dispatch(groups.0, groups.1, 1);
        pass.counted_set_pipeline(&self.pipes.atrous);
        for k in 0..iterations {
            let group = if k.is_multiple_of(2) { &to_pong } else { &to_ping };
            pass.counted_set_bind_group(0, group, &[slot_offset(k + 1)]);
            pass.counted_dispatch(groups.0, groups.1, 1);
            if k == 0 && store_first {
                pass.counted_set_bind_group(0, &to_ping, &[slot_offset(0)]);
                pass.counted_set_pipeline(&self.pipes.store);
                pass.counted_dispatch(groups.0, groups.1, 1);
                pass.counted_set_pipeline(&self.pipes.atrous);
            }
        }
        if !store_first {
            pass.counted_set_bind_group(0, &to_ping, &[slot_offset(0)]);
            pass.counted_set_pipeline(&self.pipes.store);
            pass.counted_dispatch(groups.0, groups.1, 1);
        }
        let result_group = if iterations.is_multiple_of(2) {
            &to_pong
        } else {
            &to_ping
        };
        pass.counted_set_bind_group(0, result_group, &[0]);
        pass.counted_set_pipeline(&self.pipes.composite);
        pass.counted_dispatch(
            screen_size.0.div_ceil(WORKGROUP),
            screen_size.1.div_ceil(WORKGROUP),
            1,
        );
        drop(pass);
        self.frame = self.frame.wrapping_add(1);
        self.has_history = true;
    }

    fn fill_denoise_params(&mut self, size: (u32, u32)) {
        let settings = &self.settings.denoise;
        let stride = self.denoise_stride;
        let make = |slot: u32| {
            let mut flags = 0;
            if self.has_history {
                flags |= FLAG_HISTORY;
            }
            if settings.history_from_first_pass && settings.atrous_iterations > 0 {
                flags |= FLAG_STORE_FIRST_PASS;
            }
            DenoiseParamsGpu {
                size: [size.0, size.1, 1 << slot, flags],
                temporal: [
                    settings.alpha_color,
                    settings.alpha_moments,
                    settings.max_history,
                    settings.depth_tolerance,
                ],
                edge: [
                    settings.normal_tolerance,
                    settings.sigma_depth,
                    settings.depth_epsilon,
                    settings.sigma_normal,
                ],
                extra: [
                    settings.sigma_luminance,
                    settings.spatial_variance_frames,
                    settings.variance_radius as f32,
                    0.0,
                ],
                antilag: [
                    settings.antilag_sigma,
                    settings.antilag_block as f32,
                    size.0.div_ceil(settings.antilag_block) as f32,
                    size.1.div_ceil(settings.antilag_block) as f32,
                ],
            }
        };
        for slot in 0..PARAM_SLOTS {
            let params = make(slot as u32);
            let start = (slot * stride) as usize;
            self.denoise_bytes[start..start + size_of::<DenoiseParamsGpu>()]
                .copy_from_slice(bytemuck::bytes_of(&params));
        }
    }

    /// Replaces the entire per-size owner, including both parity groups. Kept independent
    /// of ray-query pipelines so the ownership contract can also be tested on lavapipe.
    fn ensure_resources(
        device: &TrackedDevice,
        size: (u32, u32),
        resources: &mut Option<Resources>,
        generation: &mut u64,
        layout: &wgpu::BindGroupLayout,
        params: &wgpu::Buffer,
    ) -> bool {
        if resources.as_ref().is_some_and(|r| r.size == size) {
            return false;
        }
        *resources = Some(Self::build_resources(device, size, layout, params));
        *generation += 1;
        true
    }

    fn build_resources(
        device: &TrackedDevice,
        size: (u32, u32),
        layout: &wgpu::BindGroupLayout,
        params: &wgpu::Buffer,
    ) -> Resources {
        let pixels = u64::from(size.0) * u64::from(size.1);
        let buffer = |label: &str, slots: u64, extra: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (pixels * slots).max(16),
                usage: wgpu::BufferUsages::STORAGE | extra,
                mapped_at_creation: false,
            })
        };
        let reservoirs = [
            buffer(
                "restir gi reservoir 0",
                RESERVOIR_BYTES,
                wgpu::BufferUsages::empty(),
            ),
            buffer(
                "restir gi reservoir 1",
                RESERVOIR_BYTES,
                wgpu::BufferUsages::empty(),
            ),
        ];
        let temporal = buffer("restir gi temporal", RESERVOIR_BYTES, wgpu::BufferUsages::empty());
        let frame_input = buffer(
            "restir gi denoise input",
            INPUT_SLOTS * VEC4,
            wgpu::BufferUsages::empty(),
        );
        let history = buffer(
            "restir gi denoise history",
            HISTORY_SLOTS * VEC4,
            wgpu::BufferUsages::empty(),
        );
        let acc = buffer(
            "restir gi denoise acc",
            ACC_SLOTS * VEC4,
            wgpu::BufferUsages::empty(),
        );
        let blocks = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("restir gi denoise blocks"),
            size: u64::from(size.0.div_ceil(2)) * u64::from(size.1.div_ceil(2)) * VEC4,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let ping = buffer("restir gi denoise ping", VEC4, wgpu::BufferUsages::empty());
        let pong = buffer("restir gi denoise pong", VEC4, wgpu::BufferUsages::empty());
        let stats = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("restir gi stats"),
            size: STATS_BYTES,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let stats_staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("restir gi stats readback"),
            size: STATS_BYTES,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let denoise_group = |src: &wgpu::Buffer, dst: &wgpu::Buffer, label: &str| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: params,
                            offset: 0,
                            size: wgpu::BufferSize::new(size_of::<DenoiseParamsGpu>() as u64),
                        }),
                    },
                    buf(1, &frame_input),
                    buf(2, &history),
                    buf(3, &acc),
                    buf(4, src),
                    buf(5, dst),
                    buf(6, &blocks),
                ],
            })
        };
        let to_ping = denoise_group(&pong, &ping, "restir gi denoise to ping");
        let to_pong = denoise_group(&ping, &pong, "restir gi denoise to pong");
        Resources {
            size,
            reservoirs,
            temporal,
            frame_input,
            _denoise_history: history,
            _acc: acc,
            _blocks: blocks,
            _ping: ping,
            _pong: pong,
            stats,
            stats_staging,
            stats_state: StatsReadback::Idle,
            groups: [None, None],
            to_ping,
            to_pong,
        }
    }

    fn build_restir_group(
        device: &TrackedDevice,
        layout: &wgpu::BindGroupLayout,
        composite_layout: &wgpu::BindGroupLayout,
        params: &wgpu::Buffer,
        inputs: &RestirGiInputs<'_>,
        resources: &Resources,
        parity: usize,
    ) -> (wgpu::BindGroup, wgpu::BindGroup) {
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("restir gi"),
            layout,
            entries: &[
                buf(0, params),
                view(1, inputs.depth),
                view(2, inputs.normal_roughness),
                view(3, inputs.motion),
                view(4, inputs.scene_depth),
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: inputs.tlas.as_binding(),
                },
                buf(6, inputs.ddgi_uniform),
                buf(7, inputs.ddgi_data),
                buf(8, inputs.materials),
                buf(9, &resources.reservoirs[parity ^ 1]),
                buf(10, &resources.temporal),
                buf(11, &resources.reservoirs[parity]),
                buf(12, &resources.frame_input),
                buf(13, &resources.stats),
            ],
        });
        let composite = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("restir gi composite"),
            layout: composite_layout,
            entries: &[
                buf(0, params),
                view(4, inputs.scene_depth),
                buf(6, inputs.ddgi_uniform),
                buf(7, inputs.ddgi_data),
            ],
        });
        (group, composite)
    }
}

fn step_stats_readback(
    resources: &mut Resources,
    encoder: &mut wgpu::CommandEncoder,
) -> Option<RestirGiWork> {
    let mut out = None;
    if let StatsReadback::Mapping(flag) = &resources.stats_state {
        match flag.load(Ordering::Acquire) {
            MAP_PENDING => return None,
            MAP_DONE => {
                if let Ok(view) = resources.stats_staging.slice(..).get_mapped_range() {
                    let bytes: &[u8] = &view;
                    let words: Vec<u32> = bytemuck::pod_collect_to_vec(bytes);
                    if let &[
                        rays,
                        initial,
                        shadow,
                        visibility,
                        temporal,
                        spatial,
                        history,
                        validation,
                        initial_hits,
                        sky_samples,
                        sun_visible,
                        probe_lum_sum,
                        motion_rejections,
                        ..,
                    ] = words.as_slice()
                    {
                        out = Some(RestirGiWork {
                            rays: u64::from(rays),
                            initial_rays: u64::from(initial),
                            shadow_rays: u64::from(shadow),
                            visibility_rays: u64::from(visibility),
                            temporal_reused: u64::from(temporal),
                            spatial_reused: u64::from(spatial),
                            history_rejections: u64::from(history),
                            validation_rejections: u64::from(validation),
                            initial_hits: u64::from(initial_hits),
                            sky_samples: u64::from(sky_samples),
                            sun_visible: u64::from(sun_visible),
                            probe_lum_sum: u64::from(probe_lum_sum),
                            motion_rejections: u64::from(motion_rejections),
                        });
                    }
                }
                resources.stats_staging.unmap();
            }
            _ => {}
        }
        resources.stats_state = StatsReadback::Idle;
    }
    match resources.stats_state {
        StatsReadback::Idle => {
            encoder.copy_buffer_to_buffer(&resources.stats, 0, &resources.stats_staging, 0, STATS_BYTES);
            resources.stats_state = StatsReadback::Copied;
        }
        StatsReadback::Copied => {
            let flag = Arc::new(AtomicU8::new(MAP_PENDING));
            let done = flag.clone();
            resources
                .stats_staging
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |r| {
                    done.store(if r.is_ok() { MAP_DONE } else { MAP_FAILED }, Ordering::Release);
                });
            resources.stats_state = StatsReadback::Mapping(flag);
        }
        StatsReadback::Mapping(_) => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_buffers_gi_resize_releases_all_owned_groups_without_ray_query() {
        let device = crate::RenderDevice::headless().expect("lavapipe device required");
        let d = device.tracked();
        let layout = denoise_layout(&d);
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("GI lifetime test parameters"),
            size: 256,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        });
        let mut resources = None;
        let mut generation = 0;
        assert!(RestirGiGpu::ensure_resources(
            &d,
            (4, 4),
            &mut resources,
            &mut generation,
            &layout,
            &params
        ));
        assert_eq!(generation, 1);
        let r = resources.as_mut().expect("buffer audit");
        let old_pair = r.reservoirs.clone();
        let old_temporal = r.temporal.clone();
        let old_ping = r.to_ping.clone();
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("GI lifetime test key"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
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
        // Actual groups referencing all three reservoir buffers, stored in the production
        // owner. No TLAS or ray shader is needed to test their lifetime on software Vulkan.
        let probe_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("GI reservoir lifetime probe"),
            entries: &[storage(0, true), storage(1, true), storage(2, true)],
        });
        r.groups = std::array::from_fn(|parity| {
            let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("GI reservoir lifetime probe"),
                layout: &probe_layout,
                entries: &[
                    buf(0, &r.reservoirs[parity]),
                    buf(1, &r.reservoirs[parity ^ 1]),
                    buf(2, &r.temporal),
                ],
            });
            Some(RestirGroup {
                key: BindKey {
                    generation,
                    depth: view.clone(),
                    normal: view.clone(),
                    motion: view.clone(),
                    scene_depth: view.clone(),
                    ddgi_uniform: params.clone(),
                    ddgi_data: r.frame_input.clone(),
                    materials: r.frame_input.clone(),
                    scene_epoch: 0,
                },
                composite: group.clone(),
                group,
            })
        });
        assert!(r.groups.iter().all(Option::is_some));
        assert!(!RestirGiGpu::ensure_resources(
            &d,
            (4, 4),
            &mut resources,
            &mut generation,
            &layout,
            &params
        ));
        assert!(
            resources
                .as_ref()
                .expect("buffer audit")
                .groups
                .iter()
                .all(Option::is_some)
        );
        assert_eq!(generation, 1);
        assert!(RestirGiGpu::ensure_resources(
            &d,
            (8, 4),
            &mut resources,
            &mut generation,
            &layout,
            &params
        ));
        let r = resources.as_ref().expect("buffer audit");
        assert_eq!(generation, 2);
        assert!(r.groups.iter().all(Option::is_none));
        assert!(r.reservoirs.iter().all(|b| !old_pair.contains(b)));
        assert_ne!(r.temporal, old_temporal);
        assert_ne!(r.to_ping, old_ping);
        assert_eq!(r.temporal.size(), 8 * 4 * RESERVOIR_BYTES);
    }

    #[test]
    fn shader_parses_and_validates_with_ray_query_capability() {
        let module = naga::front::wgsl::parse_str(SOURCE)
            .map_err(|e| e.emit_to_string(SOURCE))
            .expect("restir_gi.wgsl parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::RAY_QUERY | naga::valid::Capabilities::RAY_HIT_VERTEX_POSITION,
        )
        .validate(&module)
        .expect("restir_gi.wgsl validates");
        for entry in [
            "gi_initial_temporal",
            "gi_spatial",
            "gi_resolve",
            "reproject",
            "antilag",
            "temporal",
            "variance",
            "atrous",
            "store_history",
            "gi_composite",
        ] {
            assert!(module.entry_points.iter().any(|e| e.name == entry), "{entry}");
        }
    }

    #[test]
    fn parameter_layouts_match_wgsl() {
        assert_eq!(size_of::<ParamsGpu>(), 160);
        assert_eq!(size_of::<DenoiseParamsGpu>(), 80);
        assert_eq!(RESERVOIR_BYTES, 96);
        assert_eq!(
            RestirGiGpu::bytes_at(64, 48),
            64 * 48 * (288 + 160) + 32 * 24 * 16
        );
    }
}
