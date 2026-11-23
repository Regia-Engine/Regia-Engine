// SPDX-License-Identifier: MPL-2.0
//! The precipitation draw: pipelines lazily per color format, one uniform buffer and the occlusion
//! binding. Instanced vertex work only, so it runs on every tier (spec 99 § 13).

use std::collections::HashMap;

use regia_core::math::Vec3;
use regia_render::{PrecipOcclusionBinding, RenderDevice};

use super::{PrecipCounts, PrecipFrame, PrecipResolution};
use crate::gpu::{VfxCamera, VfxError, VfxLighting, VfxProfile};
use crate::quality::EXPOSURE_FLOOR;

const SHADER: &str = include_str!("precip.wgsl");

/// The WGSL `cargo xtask check-shaders` validates.
#[must_use]
pub fn precip_shader_source() -> (String, String) {
    ("vfx precipitation".to_string(), SHADER.to_string())
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PrecipGpu {
    view_proj: [[f32; 4]; 4],
    occ_view_proj: [[f32; 4]; 4],
    occ_inverse: [[f32; 4]; 4],
    occ_rect: [f32; 4],
    occ_info: [f32; 4],
    box_size: [f32; 4],
    shift_clock: [f32; 4],
    eye_mod: [f32; 4],
    eye: [f32; 4],
    drift_view: [f32; 4],
    wind: [f32; 4],
    eye_velocity: [f32; 4],
    counts: [f32; 4],
    rain_fall: [[f32; 4]; 2],
    rain_speed: [[f32; 4]; 2],
    rain_diameter: [[f32; 4]; 2],
    snow_fall: [f32; 4],
    snow_speed: [f32; 4],
    right: [f32; 4],
    up: [f32; 4],
    forward: [f32; 4],
    sun_dir: [f32; 4],
    sun_color: [f32; 4],
    sky_up: [f32; 4],
    sky_down: [f32; 4],
    target_info: [f32; 4],
}

fn rows8(values: &[f32; 8]) -> [[f32; 4]; 2] {
    [
        [values[0], values[1], values[2], values[3]],
        [values[4], values[5], values[6], values[7]],
    ]
}

/// How the frame's target stores color, and what lights the drops.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrecipTarget {
    /// Viewport width in pixels.
    pub width: u32,
    /// Viewport height in pixels.
    pub height: u32,
    /// The frame's sky light rows (`VfxGpu::sky_light_rows`).
    pub sky_rows: [[f32; 4]; 2],
    /// 0 display bytes, 1 linear, 2 pre-exposure HDR radiance.
    pub target_mode: f32,
    /// Cloud cover, `0..1`: the sun through it is `1 - 0.8 coverage`.
    pub cloud_coverage: f32,
}

/// The camera and lighting of one drawn frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrecipView {
    /// The camera.
    pub camera: VfxCamera,
    /// The sun and the exposure.
    pub lighting: VfxLighting,
    /// The target.
    pub target: PrecipTarget,
}

/// One pipeline set: rain, snow, splashes.
struct Pipes {
    rain: wgpu::RenderPipeline,
    snow: wgpu::RenderPipeline,
    splash: wgpu::RenderPipeline,
}

/// The format of the reduced-resolution target ([`PrecipResolution::Half`] and `Quarter`):
/// premultiplied color in what the frame's pass would have blended.
const REDUCED_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// The reduced-resolution path (ADR-3249): the drops draw into `target` in their own pass during
/// the layer's simulate step, and the frame's pass composites it.
struct Reduced {
    /// The drops without a depth attachment (the scene depth is tested in the fragment stage).
    pipes: Pipes,
    composite_bgl: wgpu::BindGroupLayout,
    composite_layout: wgpu::PipelineLayout,
    sampler: wgpu::Sampler,
    /// Composite pipelines per frame color format.
    composite: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    /// The target, its size, and the composite's bind group over it.
    target: Option<(wgpu::TextureView, (u32, u32), wgpu::BindGroup)>,
    /// The target holds this frame's drops (recorded and not yet composited).
    ready: bool,
}

/// What this frame's resolution choice draws into and what its composite fetches. Each fragment
/// of the drops also loads the scene depth once, at any resolution; the fragment count falls with
/// the target's pixel (`tests/precip_resolution.rs` estimates it from the CPU mirror).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrecipFill {
    /// The resolution the drops drew at.
    pub resolution: PrecipResolution,
    /// The target the drops drew into, pixels.
    pub target: (u32, u32),
    /// Composite fetches: one bilinear sample per frame pixel (0 at full resolution).
    pub composite_fetches: u64,
}

/// The GPU side of the precipitation.
pub struct PrecipitationGpu {
    device: wgpu::Device,
    module: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
    bgl: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
    dummy_depth: wgpu::TextureView,
    scene_depth: Option<wgpu::TextureView>,
    occ_atlas: Option<wgpu::TextureView>,
    bind_group: Option<wgpu::BindGroup>,
    pipes: HashMap<wgpu::TextureFormat, Pipes>,
    cpu: PrecipGpu,
    instances: [u32; 3],
    /// The resolution [`Self::prepare`] set for this frame, and the frame's size.
    resolution: PrecipResolution,
    frame_size: (u32, u32),
    /// The reduced-resolution path, built on its first use.
    reduced: Option<Reduced>,
}

fn depth_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Depth,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

impl PrecipitationGpu {
    /// Creates the uniform and the shader. Pipelines build on first use, or in [`Self::warm`].
    ///
    /// # Errors
    /// Never today; keeps the shape of `VfxGpu::new`.
    pub fn new(device: &RenderDevice, _profile: &VfxProfile) -> Result<Self, VfxError> {
        let d = &device.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vfx precipitation"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx precipitation bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                depth_entry(1, wgpu::ShaderStages::FRAGMENT),
                depth_entry(2, wgpu::ShaderStages::VERTEX),
            ],
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vfx precipitation layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let uniform = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx precipitation uniform"),
            size: size_of::<PrecipGpu>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let dummy = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("vfx precipitation dummy depth"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        Ok(Self {
            device: d.clone(),
            module,
            layout,
            bgl,
            uniform,
            dummy_depth: dummy.create_view(&wgpu::TextureViewDescriptor::default()),
            scene_depth: None,
            occ_atlas: None,
            bind_group: None,
            pipes: HashMap::new(),
            cpu: bytemuck::Zeroable::zeroed(),
            instances: [0; 3],
            resolution: PrecipResolution::Full,
            frame_size: (1, 1),
            reduced: None,
        })
    }

    fn build_pipes(&self, format: wgpu::TextureFormat) -> Pipes {
        self.build_pipe_set(format, true)
    }

    /// The rain, snow and splash pipelines into `format`, with the frame's depth attachment
    /// (`depth`) or without one (the reduced target).
    fn build_pipe_set(&self, format: wgpu::TextureFormat, depth: bool) -> Pipes {
        let premult = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let build = |label: &str, vs: &str, fs: &str| {
            self.device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&self.layout),
                    vertex: wgpu::VertexState {
                        module: &self.module,
                        entry_point: Some(vs),
                        buffers: &[],
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &self.module,
                        entry_point: Some(fs),
                        targets: &[Some(wgpu::ColorTargetState {
                            format,
                            blend: Some(premult),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                        compilation_options: wgpu::PipelineCompilationOptions::default(),
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: depth.then(|| wgpu::DepthStencilState {
                        format: wgpu::TextureFormat::Depth32Float,
                        depth_write_enabled: Some(false),
                        depth_compare: Some(wgpu::CompareFunction::LessEqual),
                        stencil: wgpu::StencilState::default(),
                        bias: wgpu::DepthBiasState::default(),
                    }),
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: regia_render::pipeline_cache::cache_for(&self.device).as_deref(),
                })
        };
        if depth {
            Pipes {
                rain: build("vfx rain", "vs_rain", "fs_rain"),
                snow: build("vfx snow", "vs_snow", "fs_flake"),
                splash: build("vfx splash", "vs_splash", "fs_rain"),
            }
        } else {
            Pipes {
                rain: build("vfx rain (reduced)", "vs_rain", "fs_rain"),
                snow: build("vfx snow (reduced)", "vs_snow", "fs_flake"),
                splash: build("vfx splash (reduced)", "vs_splash", "fs_rain"),
            }
        }
    }

    /// The reduced path's pipelines, layout and sampler (once).
    fn ensure_reduced(&mut self) {
        if self.reduced.is_some() {
            return;
        }
        let _span = tracing::info_span!("vfx_precipitation_reduced_pipelines").entered();
        let d = &self.device;
        let composite_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx precipitation composite bgl"),
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
        let composite_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vfx precipitation composite layout"),
            bind_group_layouts: &[Some(&self.bgl), Some(&composite_bgl)],
            immediate_size: 0,
        });
        let sampler = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vfx precipitation composite sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let pipes = self.build_pipe_set(REDUCED_FORMAT, false);
        self.reduced = Some(Reduced {
            pipes,
            composite_bgl,
            composite_layout,
            sampler,
            composite: HashMap::new(),
            target: None,
            ready: false,
        });
    }

    /// The composite pipeline into `format` (the frame's pass, its depth attachment read-only).
    fn composite_pipeline(
        device: &wgpu::Device,
        module: &wgpu::ShaderModule,
        layout: &wgpu::PipelineLayout,
        format: wgpu::TextureFormat,
    ) -> wgpu::RenderPipeline {
        let premult = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        };
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vfx precipitation composite"),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some("vs_composite"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some("fs_composite"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: premult,
                        alpha: premult,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: regia_render::pipeline_cache::cache_for(device).as_deref(),
        })
    }

    /// Builds the reduced path's pipelines (the drops and the composite into `formats`) now, off
    /// the frame path. A project that moves a tier to `Half` or `Quarter` warms this; nothing else
    /// builds it until a frame draws at a reduced resolution.
    pub fn warm_reduced(&mut self, formats: &[wgpu::TextureFormat]) {
        self.ensure_reduced();
        let device = self.device.clone();
        let Some(reduced) = self.reduced.as_mut() else {
            return;
        };
        for &format in formats {
            if !reduced.composite.contains_key(&format) {
                let pipeline =
                    Self::composite_pipeline(&device, &self.module, &reduced.composite_layout, format);
                reduced.composite.insert(format, pipeline);
            }
        }
    }

    /// The resolution the next draw uses ([`Self::prepare`] sets it from the frame's budget).
    #[must_use]
    pub fn resolution(&self) -> PrecipResolution {
        self.resolution
    }

    /// What this frame's choice costs in composite fetches, and its target size.
    #[must_use]
    pub fn fill(&self) -> PrecipFill {
        let (w, h) = self.frame_size;
        let reduced = self.resolution != PrecipResolution::Full;
        PrecipFill {
            resolution: self.resolution,
            target: self.resolution.target_size(w, h),
            composite_fetches: if reduced { u64::from(w) * u64::from(h) } else { 0 },
        }
    }

    /// Records the reduced-resolution pass (`Half` or `Quarter`): clears the target and draws the
    /// drops into it, testing the scene depth in the fragment stage. Call once per frame after
    /// [`Self::prepare`] and [`Self::bind_depth`], before the frame's pass that calls
    /// [`Self::draw`] (the runtime layer's simulate step). At full resolution, or with nothing
    /// falling, it records nothing.
    pub fn record_reduced(&mut self, encoder: &mut wgpu::CommandEncoder) {
        if let Some(reduced) = self.reduced.as_mut() {
            reduced.ready = false;
        }
        if self.resolution == PrecipResolution::Full || self.instances == [0; 3] {
            return;
        }
        let _span = tracing::trace_span!("vfx_precipitation_reduced").entered();
        self.ensure_reduced();
        let Some(bind_group) = self.bind_group() else {
            return;
        };
        let size = self.resolution.target_size(self.frame_size.0, self.frame_size.1);
        let device = self.device.clone();
        let Some(reduced) = self.reduced.as_mut() else {
            return;
        };
        if reduced.target.as_ref().is_none_or(|(_, held, _)| *held != size) {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("vfx precipitation reduced"),
                size: wgpu::Extent3d {
                    width: size.0,
                    height: size.1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: REDUCED_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vfx precipitation composite bg"),
                layout: &reduced.composite_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&reduced.sampler),
                    },
                ],
            });
            reduced.target = Some((view, size, group));
        }
        let Some((view, _, _)) = reduced.target.as_ref() else {
            return;
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("vfx precipitation reduced"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_bind_group(0, Some(&bind_group), &[]);
        Self::draw_instances(&mut pass, &reduced.pipes, self.instances);
        reduced.ready = true;
    }

    fn draw_instances(pass: &mut wgpu::RenderPass<'_>, pipes: &Pipes, [rain, snow, splashes]: [u32; 3]) {
        if rain > 0 {
            pass.set_pipeline(&pipes.rain);
            pass.draw(0..6, 0..rain);
        }
        if snow > 0 {
            pass.set_pipeline(&pipes.snow);
            pass.draw(0..6, 0..snow);
        }
        if splashes > 0 {
            pass.set_pipeline(&pipes.splash);
            pass.draw(0..6, 0..splashes);
        }
    }

    /// Builds the pipelines for `formats` now, off the frame path. Formats already built cost
    /// nothing.
    pub fn warm(&mut self, formats: &[wgpu::TextureFormat]) {
        for &format in formats {
            if !self.pipes.contains_key(&format) {
                let pipes = self.build_pipes(format);
                self.pipes.insert(format, pipes);
            }
        }
    }

    /// Binds the frame's scene depth (the soft fade), keyed by the view's id.
    pub fn bind_depth(&mut self, depth: &wgpu::TextureView) {
        if self.scene_depth.as_ref() != Some(depth) {
            self.scene_depth = Some(depth.clone());
            self.bind_group = None;
        }
    }

    /// Binds the occlusion tile; `None` is open sky everywhere and no splashes.
    pub fn bind_occlusion(&mut self, occlusion: Option<&PrecipOcclusionBinding<'_>>) {
        match occlusion {
            Some(binding) => {
                if self.occ_atlas.as_ref() != Some(binding.atlas) {
                    self.occ_atlas = Some(binding.atlas.clone());
                    self.bind_group = None;
                }
                let vp = regia_core::glam::Mat4::from_cols_array_2d(&binding.view_proj);
                self.cpu.occ_view_proj = binding.view_proj;
                self.cpu.occ_inverse = vp.inverse().to_cols_array_2d();
                self.cpu.occ_rect = binding.uv_rect;
                self.cpu.occ_info = [binding.atlas_texels, binding.tile_texels, 1.0, 0.0];
            }
            None => {
                self.cpu.occ_info = [1.0, 1.0, 0.0, 0.0];
            }
        }
    }

    /// Uploads the frame: one uniform write. A frame with nothing to draw uploads nothing.
    pub fn prepare(&mut self, queue: &wgpu::Queue, frame: &PrecipFrame, view: &PrecipView) {
        self.instances = [
            PrecipCounts::instances(frame.counts.rain),
            PrecipCounts::instances(frame.counts.snow),
            if self.cpu.occ_info[2] > 0.5 {
                PrecipCounts::instances(frame.counts.splashes)
            } else {
                0
            },
        ];
        self.resolution = frame.budget.resolution;
        self.frame_size = (view.target.width.max(1), view.target.height.max(1));
        if frame.is_empty() {
            self.instances = [0; 3];
            return;
        }
        let camera = view.camera;
        let target = view.target;
        // The drops' pixel is the drawn target's: a reduced target widens thin streaks to its
        // own pixel, and the fragment stage scales its position back to the scene depth's.
        let (_, drawn_height) = self
            .resolution
            .target_size(target.width.max(1), target.height.max(1));
        let aspect = target.width.max(1) as f32 / target.height.max(1) as f32;
        // The same camera the effects use, the eye moved to the origin.
        let rebased = VfxCamera {
            eye: Vec3::ZERO,
            target: camera.target - camera.eye,
            ..camera
        };
        let (_, _, view_proj) = rebased.matrices(aspect);
        let forward = (camera.target - camera.eye).normalize_or_zero();
        let right = forward.cross(Vec3::Y).normalize_or(Vec3::X);
        let up = right.cross(forward).normalize_or(Vec3::Y);
        let (fov_y, near, far) =
            regia_render::view_math::usable_projection(camera.fov_y, camera.near, camera.far);
        let px_per_m = drawn_height as f32 * 0.5 / (fov_y * 0.5).tan().max(1.0e-6);
        let b = frame.budget.box_size();
        let shift = frame.center_shift_m();
        let light = view.lighting;
        let clock = super::wrapped_clock_s(frame.clock_s) as f32;
        let sun = light.sun_direction;
        let cloud = 1.0 - 0.8 * target.cloud_coverage.clamp(0.0, 1.0);
        let padded = |v: [f32; 3], w: f32| [v[0], v[1], v[2], w];
        self.cpu = PrecipGpu {
            view_proj: view_proj.to_cols_array_2d(),
            box_size: [b[0], b[1], b[2], 0.5 * frame.budget.half_extent_m],
            shift_clock: padded(shift, clock),
            eye_mod: padded(frame.eye_mod_m, frame.streak_exposure_s),
            eye: [
                frame.eye[0] as f32,
                frame.eye[1] as f32,
                frame.eye[2] as f32,
                px_per_m,
            ],
            drift_view: [
                frame.drift_m[0],
                frame.drift_m[1],
                target.width as f32,
                target.height as f32,
            ],
            wind: padded(frame.wind_m_per_s, 0.0),
            eye_velocity: padded(frame.eye_velocity_m_per_s, 0.0),
            counts: [frame.counts.rain, frame.counts.snow, frame.counts.splashes, 0.0],
            rain_fall: rows8(&frame.rain_offsets_m),
            rain_speed: rows8(&frame.rain_speeds_m_per_s),
            rain_diameter: rows8(&frame.rain_diameters_mm),
            snow_fall: frame.snow_offsets_m,
            snow_speed: crate::weather::SNOW_FALL_SPEEDS_M_PER_S,
            right: padded(right.to_array(), near),
            up: padded(up.to_array(), far),
            forward: padded(forward.to_array(), 0.0),
            sun_dir: padded(sun, cloud),
            sun_color: padded(light.sun_color, light.ambient),
            sky_up: target.sky_rows[0],
            sky_down: target.sky_rows[1],
            target_info: [
                light.exposure.max(EXPOSURE_FLOOR),
                target.target_mode,
                self.resolution.divisor() as f32,
                0.0,
            ],
            ..self.cpu
        };
        queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&self.cpu));
    }

    fn bind_group(&mut self) -> Option<wgpu::BindGroup> {
        if self.bind_group.is_none() {
            let depth = self.scene_depth.clone()?;
            let atlas = self.occ_atlas.clone().unwrap_or_else(|| self.dummy_depth.clone());
            self.bind_group = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vfx precipitation bg"),
                layout: &self.bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.uniform.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&depth),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&atlas),
                    },
                ],
            }));
        }
        self.bind_group.clone()
    }

    /// Instances of `[rain, snow, splashes]` the next draw records.
    #[must_use]
    pub fn instances(&self) -> [u32; 3] {
        self.instances
    }

    /// Draws what [`Self::prepare`] uploaded into `pass`. Records nothing when nothing falls.
    pub fn draw(&mut self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat) {
        if self.instances == [0; 3] {
            return;
        }
        let _span = tracing::trace_span!("vfx_precipitation").entered();
        let Some(bind_group) = self.bind_group() else {
            return;
        };
        if self.resolution != PrecipResolution::Full {
            self.composite(pass, format, &bind_group);
            return;
        }
        if !self.pipes.contains_key(&format) {
            self.warm(&[format]);
        }
        let Some(pipes) = self.pipes.get(&format) else {
            return;
        };
        pass.set_bind_group(0, Some(&bind_group), &[]);
        Self::draw_instances(pass, pipes, self.instances);
    }

    /// Composites the reduced target [`Self::record_reduced`] filled this frame over `pass`.
    fn composite(
        &mut self,
        pass: &mut wgpu::RenderPass<'_>,
        format: wgpu::TextureFormat,
        bind_group: &wgpu::BindGroup,
    ) {
        let ready = self
            .reduced
            .as_ref()
            .is_some_and(|r| r.ready && r.target.is_some());
        if !ready {
            return;
        }
        if self
            .reduced
            .as_ref()
            .is_some_and(|r| !r.composite.contains_key(&format))
        {
            self.warm_reduced(&[format]);
        }
        let Some(reduced) = self.reduced.as_ref() else {
            return;
        };
        let (Some(pipeline), Some((_, _, group))) = (reduced.composite.get(&format), reduced.target.as_ref())
        else {
            return;
        };
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, Some(bind_group), &[]);
        pass.set_bind_group(1, Some(group), &[]);
        pass.draw(0..3, 0..1);
    }
}

/// Contract rows of the precipitation uniform (`regia_render::ContractRow`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<regia_render::ContractRow> {
    vec![regia_render::contract_row!(
        // The name `shader_sources` gives the module (ADR-5802).
        "vfx precipitation",
        "Precip",
        PrecipGpu {
            view_proj,
            occ_view_proj,
            occ_inverse,
            occ_rect,
            occ_info,
            box_size,
            shift_clock,
            eye_mod,
            eye,
            drift_view,
            wind,
            eye_velocity,
            counts,
            rain_fall,
            rain_speed,
            rain_diameter,
            snow_fall,
            snow_speed,
            right,
            up,
            forward,
            sun_dir,
            sun_color,
            sky_up,
            sky_down,
            target_info
        }
    )]
}
