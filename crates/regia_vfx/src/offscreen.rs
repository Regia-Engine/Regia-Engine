// SPDX-License-Identifier: MPL-2.0
//! The offscreen VFX harness: a lit ground + sky backdrop with VFX composited on top.
//!
//! This is the reference consumer of the [`crate::gpu`] draw hook (the same role
//! `regia_terrain::gpu::render_offscreen` plays for terrain): it owns a [`VfxGpu`], renders
//! a simple deterministic backdrop (gradient sky + Lambert ground plane that writes the
//! scene depth), then runs the VFX compute passes and draws decals, particles and trails in
//! a transparent pass over it. The result is tightly packed `Rgba8UnormSrgb` bytes, row-major,
//! top row first — the input of [`crate::measure`].
//!
//! Used by `samples/vfx_lab --evidence` (PNG evidence in `docs/evidence/m14/`) and the
//! offscreen GPU tests; the same frame loop measures the § 6 perf budget through
//! [`GpuTimer`] pass timestamps (`vfx_lab --bench`).

use regia_core::math::Vec3;
use regia_render::RenderDevice;

use crate::gpu::{VfxCamera, VfxError, VfxGpu, VfxLighting, VfxProfile};
use crate::timing::GpuTimer;

/// The harness display format (evidence PNGs). The attachment encodes linear shader output.
pub const HARNESS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Scene + particle target. `Rgba16Float` blends on Vulkan; `Rgba32Float` does not, so cores
/// above 1 live here and the present pass tonemaps them into [`HARNESS_FORMAT`].
pub const HARNESS_HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Ground plane: 240 m across, 4 m quads (64 segments per side).
const GROUND_EXTENT: f32 = 240.0;
const GROUND_SEGMENTS: u32 = 64;

/// Scene-to-sun direction (the disc sits in the sky). Not the travel direction.
const SUN_DIR: Vec3 = Vec3::new(-0.45, 0.62, 0.42);

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct HarnessUniform {
    view_proj: [[f32; 4]; 4],
    inv_view_proj: [[f32; 4]; 4],
    cam_pos: [f32; 4],
    sun_dir: [f32; 4],
    sun_color: [f32; 4],
    params: [f32; 4],
    /// xyz world position, w radius. Radius 0 leaves the ground unlit by this light.
    point: [f32; 4],
    /// Linear RGB times intensity.
    point_color: [f32; 4],
}

/// One rendered offscreen frame.
#[derive(Debug, Clone)]
pub struct OffscreenFrame {
    /// Tightly packed RGBA8 (sRGB-encoded), width × height, top row first.
    pub pixels: Vec<u8>,
    /// Frame width.
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// GPU list counters after the frame: `(additive, alpha, mesh, alive)`.
    pub counters: [u32; 4],
    /// Linear pre-tonemap RGBA (`Rgba16Float` decoded) when [`VfxHarness::set_capture_hdr`] is set.
    pub hdr: Option<Vec<f32>>,
}

/// The harness: backdrop pipelines + targets around one [`VfxGpu`].
pub struct VfxHarness {
    gpu: VfxGpu,
    uniform_buf: wgpu::Buffer,
    bg: wgpu::BindGroup,
    sky_pipe: wgpu::RenderPipeline,
    ground_pipe: wgpu::RenderPipeline,
    ground_vb: wgpu::Buffer,
    ground_ib: wgpu::Buffer,
    ground_indices: u32,
    present_pipe: wgpu::RenderPipeline,
    present_bgl: wgpu::BindGroupLayout,
    present_bg: Option<wgpu::BindGroup>,
    bloom: bool,
    capture_hdr: bool,
    lighting: VfxLighting,
    point: [f32; 4],
    point_color: [f32; 4],
    // cached per-size targets (the harness reuses them across frames)
    /// Display target (`Rgba8UnormSrgb`).
    colour: Option<(u32, u32, wgpu::Texture, wgpu::TextureView)>,
    /// Linear HDR scene (`Rgba16Float`).
    hdr: Option<(u32, u32, wgpu::Texture, wgpu::TextureView)>,
    /// Copy of the HDR scene after decals, before particles (heat-haze source).
    haze: Option<(u32, u32, wgpu::Texture, wgpu::TextureView)>,
    depth: Option<(u32, u32, wgpu::Texture, wgpu::TextureView)>,
    readback: Option<(u32, u32, u32, wgpu::Buffer)>, // (w, h, padded_row, buffer)
    hdr_readback: Option<(u32, u32, u32, wgpu::Buffer)>,
    /// The rain and snow draw ([`VfxHarness::set_precipitation`]).
    precip: Option<crate::precip::PrecipitationGpu>,
    precip_frame: Option<crate::precip::PrecipFrame>,
}

/// Display bytes (sRGB RGBA8) and optional linear HDR (`Rgba16Float` decoded to f32).
type HarnessReadback = (Option<Vec<u8>>, Option<Vec<f32>>);

impl VfxHarness {
    /// Creates the harness (sky + ground pipelines, buffers) around a fresh [`VfxGpu`] for
    /// `profile`.
    pub fn new(device: &RenderDevice, profile: VfxProfile) -> Result<Self, VfxError> {
        let d = &device.device;
        let gpu = VfxGpu::new(device, profile)?;

        let uniform_buf = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx harness uniform"),
            size: size_of::<HarnessUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx harness bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vfx harness bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        // Ground grid.
        let step = GROUND_EXTENT / GROUND_SEGMENTS as f32;
        let half = GROUND_EXTENT * 0.5;
        let mut verts: Vec<[f32; 3]> = Vec::new();
        for row in 0..=GROUND_SEGMENTS {
            for col in 0..=GROUND_SEGMENTS {
                let x = -half + col as f32 * step;
                let z = -half + row as f32 * step;
                verts.push([x, 0.0, z]);
            }
        }
        let mut indices: Vec<u32> = Vec::new();
        for row in 0..GROUND_SEGMENTS {
            for col in 0..GROUND_SEGMENTS {
                let a = row * (GROUND_SEGMENTS + 1) + col;
                let b = a + 1;
                let c = a + GROUND_SEGMENTS + 1;
                let dd = c + 1;
                // CCW seen from +Y
                indices.extend_from_slice(&[a, dd, b, a, c, dd]);
            }
        }
        let ground_indices = indices.len() as u32;
        let ground_vb = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx harness ground vb"),
            size: (verts.len() * 12) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        device
            .queue
            .write_buffer(&ground_vb, 0, bytemuck::cast_slice(&verts));
        let ground_ib = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx harness ground ib"),
            size: (indices.len() * 4) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        device
            .queue
            .write_buffer(&ground_ib, 0, bytemuck::cast_slice(&indices));

        // Pipelines.
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vfx harness"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu/harness.wgsl").to_string().into()),
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vfx harness layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let error_scope = d.push_error_scope(wgpu::ErrorFilter::Validation);
        let sky_pipe = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vfx harness sky"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_sky"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_sky"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: HARNESS_HDR_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            // Passes carry the ground's depth attachment; the sky ignores depth but must
            // declare the format to stay compatible with the pass.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
        });
        let ground_pipe = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vfx harness ground"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_ground"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: 12,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x3,
                        offset: 0,
                        shader_location: 0,
                    }],
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_ground"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: HARNESS_HDR_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
        });
        let present_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx harness present bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let present_module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vfx harness present"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu/present.wgsl").to_string().into()),
        });
        let present_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vfx harness present layout"),
            bind_group_layouts: &[Some(&present_bgl)],
            immediate_size: 0,
        });
        let present_pipe = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vfx harness present"),
            layout: Some(&present_layout),
            vertex: wgpu::VertexState {
                module: &present_module,
                entry_point: Some("vs_present"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &present_module,
                entry_point: Some("fs_present"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: HARNESS_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: regia_render::pipeline_cache::cache_for(&d).as_deref(),
        });
        let validation = regia_core::tasks::block_on(error_scope.pop());
        if let Some(err) = validation {
            return Err(VfxError::Shader(err.to_string()));
        }

        Ok(Self {
            precip: None,
            precip_frame: None,
            gpu,
            uniform_buf,
            bg,
            sky_pipe,
            ground_pipe,
            ground_vb,
            ground_ib,
            ground_indices,
            present_pipe,
            present_bgl,
            present_bg: None,
            bloom: true,
            capture_hdr: false,
            lighting: VfxLighting::default(),
            point: [0.0; 4],
            point_color: [0.0; 4],
            colour: None,
            hdr: None,
            depth: None,
            readback: None,
            hdr_readback: None,
            haze: None,
        })
    }

    /// Bloom of HDR luma above 1. On by default so fireball cores feed the present halo.
    pub fn set_bloom(&mut self, enabled: bool) {
        self.bloom = enabled;
    }

    /// Keep a linear pre-tonemap copy on the next [`Self::render`] (`OffscreenFrame::hdr`).
    pub fn set_capture_hdr(&mut self, enabled: bool) {
        self.capture_hdr = enabled;
    }

    /// Particle sun and ambient. The sky disc stays on the harness sun; this only changes
    /// how puffs are shaded. Defaults match that sun.
    pub fn set_lighting(&mut self, lighting: VfxLighting) {
        self.lighting = lighting;
    }

    /// One particle point light on the ground. `radius` 0 turns it off.
    ///
    /// `color` is linear RGB already scaled by intensity. The ground shader uses the
    /// windowed inverse-square from `pbr_lighting.wgsl`.
    pub fn set_point_light(&mut self, position: Vec3, color: [f32; 3], radius: f32) {
        self.point = [position.x, position.y, position.z, radius.max(0.0)];
        self.point_color = [color[0], color[1], color[2], 0.0];
        // The same light reaches lit puffs (smoke over the fire).
        let light = (radius > 0.0).then_some((position, color, radius));
        self.gpu.set_fire_light(light);
    }

    /// Draws `frame`'s rain and snow in the next renders, before the particles; `None` draws
    /// none (spec 99 § 5).
    pub fn set_precipitation(&mut self, device: &RenderDevice, frame: Option<crate::precip::PrecipFrame>) {
        if frame.is_some() && self.precip.is_none() {
            self.precip = crate::precip::PrecipitationGpu::new(device, &VfxProfile::default()).ok();
        }
        self.precip_frame = frame;
        if frame.is_none() {
            self.precip = None;
        }
    }

    /// The VFX context (spawn jobs, decals and trails are uploaded through this).
    pub fn gpu(&mut self) -> &mut VfxGpu {
        &mut self.gpu
    }

    /// Renders one frame: sky + ground, then VFX (simulate + decals + particles + trails),
    /// and reads the pixels back.
    ///
    /// `dt` advances the GPU simulation; the caller drives the CPU scene separately
    /// ([`crate::scene::VfxScene::update`] + [`VfxScene::upload`](crate::scene::VfxScene::upload))
    /// before calling this.
    pub fn render(
        &mut self,
        device: &RenderDevice,
        camera: &VfxCamera,
        width: u32,
        height: u32,
        dt: f32,
    ) -> Result<OffscreenFrame, VfxError> {
        let mut counters = [0u32; 4];
        let (pixels, hdr) =
            self.render_inner(device, camera, width, height, dt, None, &mut counters, true)?;
        Ok(OffscreenFrame {
            pixels: pixels.unwrap_or_default(),
            width,
            height,
            counters,
            hdr,
        })
    }

    /// Renders one frame with GPU pass timestamps (perf runs); skips the pixel readback.
    ///
    /// Timestamp slots: 0–1 = VFX compute pass, 2–3 = VFX draw pass. Call
    /// [`GpuTimer::resolve`] is done inside; read with [`GpuTimer::read_ms`] after.
    pub fn render_timed(
        &mut self,
        device: &RenderDevice,
        camera: &VfxCamera,
        width: u32,
        height: u32,
        dt: f32,
        timer: &GpuTimer,
    ) -> Result<[u32; 4], VfxError> {
        let mut counters = [0u32; 4];
        self.render_inner(
            device,
            camera,
            width,
            height,
            dt,
            Some(timer),
            &mut counters,
            false,
        )?;
        Ok(counters)
    }

    /// Renders the frame, optionally timing the VFX passes; `readback` selects pixel readback.
    #[allow(clippy::too_many_arguments)]
    fn render_inner(
        &mut self,
        device: &RenderDevice,
        camera: &VfxCamera,
        width: u32,
        height: u32,
        dt: f32,
        timer: Option<&GpuTimer>,
        counters: &mut [u32; 4],
        readback: bool,
    ) -> Result<HarnessReadback, VfxError> {
        let d = &device.device;
        let width = width.max(1);
        let height = height.max(1);
        crate::gpu::check_texture_extent(width, height, d.limits().max_texture_dimension_2d)?;
        self.ensure_targets(d, width, height);

        // Frame uniform (matches the vfx camera exactly — the backdrop and VFX must share it).
        let aspect = width as f32 / height as f32;
        let (_view, _proj, view_proj) = camera.matrices(aspect);
        let sun = SUN_DIR.normalize_or(Vec3::Y);
        let uniform = HarnessUniform {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            cam_pos: [camera.eye.x, camera.eye.y, camera.eye.z, 0.0],
            sun_dir: [sun.x, sun.y, sun.z, 0.0],
            sun_color: [1.05, 0.98, 0.85, 0.5],
            params: [0.0055, 2.0, if self.bloom { 1.0 } else { 0.0 }, 0.0],
            point: self.point,
            point_color: self.point_color,
        };
        device
            .queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniform));

        // VFX frame: camera, then simulate after the backdrop pass recorded below.
        self.gpu.set_lighting(self.lighting);
        let scene_depth = self.depth_view();
        if let Some(frame) = self.precip_frame
            && let Some(precip) = self.precip.as_mut()
        {
            precip.bind_depth(&scene_depth);
            precip.prepare(
                &device.queue,
                &frame,
                &crate::precip::PrecipView {
                    camera: *camera,
                    lighting: self.lighting,
                    target: crate::precip::PrecipTarget {
                        width,
                        height,
                        sky_rows: [[0.0; 4]; 2],
                        target_mode: 2.0,
                        cloud_coverage: 0.5,
                    },
                },
            );
        }
        let haze_view = self.haze.as_ref().map(|h| h.3.clone());
        self.gpu.set_haze_source(haze_view.as_ref());
        self.gpu.set_depth_view(Some(&self.depth_view()));
        // The harness draws into an HDR target: the profile's soft-particle layer applies.
        self.gpu.set_soft_pass(true);
        self.gpu.set_frame(camera, dt, width, height);
        self.gpu.set_colour_format(HARNESS_FORMAT);

        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("vfx harness frame"),
        });

        // Pass 1: sky (background, no depth) + ground (writes the scene depth).
        {
            let _span = tracing::trace_span!("vfx_harness_backdrop").entered();
            let colour = self.hdr_view();
            let depth = self.depth_view();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vfx harness backdrop"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &colour,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.02,
                            g: 0.03,
                            b: 0.05,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, Some(&self.bg), &[]);
            pass.set_pipeline(&self.sky_pipe);
            pass.draw(0..3, 0..1);
            pass.set_pipeline(&self.ground_pipe);
            pass.set_vertex_buffer(0, self.ground_vb.slice(..));
            pass.set_index_buffer(self.ground_ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..self.ground_indices, 0, 0..1);
        }

        // VFX compute: emit + update + sort + indirect prep.
        {
            let _span = tracing::trace_span!("vfx_harness_simulate").entered();
            self.gpu.simulate_timed(
                &mut encoder,
                timer.map(|t| wgpu::ComputePassTimestampWrites {
                    query_set: t.query_set(),
                    beginning_of_pass_write_index: Some(0),
                    end_of_pass_write_index: Some(1),
                }),
            );
            self.gpu.record_stats_copy(&mut encoder);
        }

        // Pass 2: transparent VFX over the backdrop (depth read-only — the shaders sample it).
        // Decals first, then a copy of the HDR scene for heat haze, then particles + trails.
        // The draw-slot timestamp (slots 2–3) spans everything the VFX draw pass does: decals,
        // the sorted lists, and — with the opt-in half-res soft-particle pass — the half-res
        // alpha draw and its bilateral upsample, so the budget quoted from slot 2–3 includes
        // them rather than measuring an empty non-alpha list.
        {
            let _span = tracing::trace_span!("vfx_harness_draw").entered();
            let colour = self.hdr_view();
            let depth = self.depth_view();
            let attachments = || {
                (
                    [Some(wgpu::RenderPassColorAttachment {
                        view: &colour,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &depth,
                        depth_ops: None, // read-only depth: also sampled as a texture
                        stencil_ops: None,
                    }),
                )
            };
            {
                let (colour_att, depth_att) = attachments();
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("vfx harness decals"),
                    color_attachments: &colour_att,
                    depth_stencil_attachment: depth_att,
                    timestamp_writes: timer.map(|t| wgpu::RenderPassTimestampWrites {
                        query_set: t.query_set(),
                        beginning_of_pass_write_index: Some(2),
                        end_of_pass_write_index: None,
                    }),
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                self.gpu.draw_decals(&mut pass, HARNESS_HDR_FORMAT)?;
            }
            // Only when a haze card is live: otherwise a full-screen copy a frame for nothing.
            if self.gpu.wants_haze()
                && let (Some((_, _, src, _)), Some((_, _, dst, _))) = (self.hdr.as_ref(), self.haze.as_ref())
            {
                encoder.copy_texture_to_texture(
                    src.as_image_copy(),
                    dst.as_image_copy(),
                    wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                );
            }
            // The low-resolution soft layer (dust, smoke, billows), inside the draw slot.
            self.gpu.record_soft_pass(&mut encoder, width, height)?;
            // Rain and snow at `Half` or `Quarter` draw into their own target first; the pass
            // below composites it (records nothing at full resolution).
            if let Some(precip) = self.precip.as_mut() {
                precip.record_reduced(&mut encoder);
            }
            let (colour_att, depth_att) = attachments();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vfx harness vfx"),
                color_attachments: &colour_att,
                depth_stencil_attachment: depth_att,
                // The draw slot (2–3) spans decals, the soft layer, the full-res lists and the
                // soft composite.
                timestamp_writes: timer.map(|t| wgpu::RenderPassTimestampWrites {
                    query_set: t.query_set(),
                    beginning_of_pass_write_index: None,
                    end_of_pass_write_index: Some(3),
                }),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(precip) = self.precip.as_mut() {
                precip.draw(&mut pass, HARNESS_HDR_FORMAT);
            }
            self.gpu.draw_particles(&mut pass, HARNESS_HDR_FORMAT)?;
            self.gpu.draw_trails(&mut pass, HARNESS_HDR_FORMAT);
            self.gpu.draw_soft_composite(&mut pass, HARNESS_HDR_FORMAT)?;
        }

        // Pass 3: tonemap (+ optional bloom) into the 8-bit display target.
        {
            let _span = tracing::trace_span!("vfx_harness_present").entered();
            let colour = self.colour_view();
            let bg = self
                .present_bg
                .as_ref()
                .ok_or_else(|| VfxError::Shader("present bind group missing".into()))?;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vfx harness present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &colour,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, Some(bg), &[]);
            pass.set_pipeline(&self.present_pipe);
            pass.draw(0..3, 0..1);
        }

        let capture = readback && self.capture_hdr;
        if capture {
            self.copy_hdr(d, &mut encoder, width, height);
        }
        if readback {
            self.copy_pixels(d, &mut encoder, width, height);
        }
        if let Some(timer) = timer {
            timer.resolve(&mut encoder, 4);
        }
        device.queue.submit([encoder.finish()]);

        if let Some(c) = self.gpu.read_stats() {
            *counters = c;
        } else if !readback {
            return Err(VfxError::Shader("stats readback failed".into()));
        }
        if readback {
            let pixels = self.read_pixels(d, width, height)?;
            let hdr = if capture {
                Some(self.read_hdr(d, width, height)?)
            } else {
                None
            };
            Ok((pixels, hdr))
        } else {
            Ok((None, None))
        }
    }

    fn ensure_targets(&mut self, d: &wgpu::Device, width: u32, height: u32) {
        let size = |w: u32, h: u32| wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        if self
            .colour
            .as_ref()
            .is_none_or(|(w, h, _, _)| *w != width || *h != height)
        {
            let texture = d.create_texture(&wgpu::TextureDescriptor {
                label: Some("vfx harness color"),
                size: size(width, height),
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: HARNESS_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.colour = Some((width, height, texture, view));
        }
        if self
            .hdr
            .as_ref()
            .is_none_or(|(w, h, _, _)| *w != width || *h != height)
        {
            let texture = d.create_texture(&wgpu::TextureDescriptor {
                label: Some("vfx harness hdr"),
                size: size(width, height),
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: HARNESS_HDR_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.present_bg = Some(d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vfx harness present bg"),
                layout: &self.present_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.uniform_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                ],
            }));
            self.hdr = Some((width, height, texture, view));
            let haze = d.create_texture(&wgpu::TextureDescriptor {
                label: Some("vfx harness haze copy"),
                size: size(width, height),
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: HARNESS_HDR_FORMAT,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let haze_view = haze.create_view(&wgpu::TextureViewDescriptor::default());
            self.haze = Some((width, height, haze, haze_view));
        }
        if self
            .depth
            .as_ref()
            .is_none_or(|(w, h, _, _)| *w != width || *h != height)
        {
            let texture = d.create_texture(&wgpu::TextureDescriptor {
                label: Some("vfx harness depth"),
                size: size(width, height),
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.depth = Some((width, height, texture, view));
        }
    }

    fn colour_view(&self) -> wgpu::TextureView {
        self.colour.as_ref().expect("targets ensured").3.clone()
    }

    fn hdr_view(&self) -> wgpu::TextureView {
        self.hdr.as_ref().expect("targets ensured").3.clone()
    }

    fn depth_view(&self) -> wgpu::TextureView {
        self.depth.as_ref().expect("targets ensured").3.clone()
    }

    fn copy_pixels(&mut self, d: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, width: u32, height: u32) {
        let row = width * 4;
        let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        if self
            .readback
            .as_ref()
            .is_none_or(|(w, h, p, _)| *w != width || *h != height || *p != padded)
        {
            self.readback = Some((
                width,
                height,
                padded,
                d.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("vfx harness readback"),
                    size: padded as u64 * height as u64,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
            ));
        }
        let buffer = self.readback.as_ref().expect("just set").3.clone();
        let texture = self.colour.as_ref().expect("targets ensured").2.clone();
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }

    fn read_pixels(&self, d: &wgpu::Device, width: u32, height: u32) -> Result<Option<Vec<u8>>, VfxError> {
        let (_, _, padded, buffer) = self.readback.as_ref().expect("copy_pixels ran");
        let (tx, rx) = std::sync::mpsc::channel();
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        // Unmap on every path after the map was requested: a buffer left mapped fails every later
        // `map_async` on it.
        let result = (|| -> Result<Option<Vec<u8>>, VfxError> {
            d.poll(wgpu::PollType::wait_indefinitely())
                .map_err(|e| VfxError::Shader(format!("readback poll: {e}")))?;
            rx.recv()
                .map_err(|_| VfxError::Shader("readback channel".into()))?
                .map_err(|e| VfxError::Shader(format!("readback map: {e}")))?;
            let mapped = slice
                .get_mapped_range()
                .map_err(|_| VfxError::Shader("readback range".into()))?;
            let row_bytes = (width * 4) as usize;
            let mut pixels = Vec::with_capacity(row_bytes * height as usize);
            for row in 0..height as usize {
                let start = row * *padded as usize;
                pixels.extend_from_slice(&mapped[start..start + row_bytes]);
            }
            Ok(Some(pixels))
        })();
        buffer.unmap();
        result
    }

    fn copy_hdr(&mut self, d: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, width: u32, height: u32) {
        let row = width * 8;
        let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        if self
            .hdr_readback
            .as_ref()
            .is_none_or(|(w, h, p, _)| *w != width || *h != height || *p != padded)
        {
            self.hdr_readback = Some((
                width,
                height,
                padded,
                d.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("vfx harness hdr readback"),
                    size: padded as u64 * height as u64,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
            ));
        }
        let buffer = self.hdr_readback.as_ref().expect("just set").3.clone();
        let texture = self.hdr.as_ref().expect("targets ensured").2.clone();
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }

    fn read_hdr(&self, d: &wgpu::Device, width: u32, height: u32) -> Result<Vec<f32>, VfxError> {
        let (_, _, padded, buffer) = self.hdr_readback.as_ref().expect("copy_hdr ran");
        let (tx, rx) = std::sync::mpsc::channel();
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        // Unmap on every path after the map was requested (a mapped buffer fails every later
        // `map_async`).
        let result = (|| -> Result<Vec<f32>, VfxError> {
            d.poll(wgpu::PollType::wait_indefinitely())
                .map_err(|e| VfxError::Shader(format!("hdr readback poll: {e}")))?;
            rx.recv()
                .map_err(|_| VfxError::Shader("hdr readback channel".into()))?
                .map_err(|e| VfxError::Shader(format!("hdr readback map: {e}")))?;
            let mapped = slice
                .get_mapped_range()
                .map_err(|_| VfxError::Shader("hdr readback range".into()))?;
            let mut pixels = Vec::with_capacity((width * height * 4) as usize);
            for row in 0..height as usize {
                let start = row * *padded as usize;
                let bytes = &mapped[start..start + (width as usize) * 8];
                for px in bytes.as_chunks::<2>().0 {
                    pixels.push(f16_to_f32(u16::from_le_bytes(*px)));
                }
            }
            Ok(pixels)
        })();
        buffer.unmap();
        result
    }
}

/// IEEE-754 binary16 → binary32. HDR readback is `Rgba16Float`.
fn f16_to_f32(h: u16) -> f32 {
    let sign = (h >> 15) & 1;
    let exp = (h >> 10) & 0x1f;
    let frac = h & 0x3ff;
    let bits = if exp == 0 {
        if frac == 0 {
            u32::from(sign) << 31
        } else {
            let mut f = u32::from(frac);
            let mut e: u32 = 127 - 15 + 1;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            f &= 0x3ff;
            (u32::from(sign) << 31) | (e << 23) | (f << 13)
        }
    } else if exp == 31 {
        (u32::from(sign) << 31) | (0xff << 23) | (u32::from(frac) << 13)
    } else {
        (u32::from(sign) << 31) | ((u32::from(exp) + 127 - 15) << 23) | (u32::from(frac) << 13)
    };
    f32::from_bits(bits)
}

/// Contract rows of the harness uniform (`regia_render::ContractRow`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<regia_render::ContractRow> {
    vec![regia_render::contract_row!(
        // The names the shader lists give these modules (`gpu::shader_sources`, xtask's
        // `plain_sources`); file names here matched nothing (ADR-5802).
        "vfx harness,vfx present.wgsl",
        "Harness",
        HarnessUniform {
            view_proj,
            inv_view_proj,
            cam_pos,
            sun_dir,
            sun_color,
            params,
            point,
            point_color
        }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The backdrop alone must be healthy: not black, not blown out, and the sun must
    /// light the ground (warmth > 0 comes from the effects, so here just sanity bands).
    #[test]
    fn backdrop_is_sensibly_exposed() {
        let device = match RenderDevice::shared_headless() {
            Ok(d) => d,
            Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
        };
        let mut harness =
            VfxHarness::new(&device, VfxProfile::desktop_high()).expect("harness builds (shader validation)");
        let camera = VfxCamera::looking_at(Vec3::new(0.0, 2.0, 7.0), Vec3::new(0.0, 1.0, 0.0));
        let frame = harness
            .render(&device, &camera, 320, 180, 1.0 / 60.0)
            .expect("backdrop renders");
        let luma = |p: [u8; 4]| {
            (0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2])) / 255.0
        };
        let px = frame.pixels.as_chunks::<4>().0;
        let mean = px.iter().map(|p| luma(*p)).sum::<f32>() / px.len() as f32;
        let max = px.iter().map(|p| luma(*p)).fold(0.0f32, f32::max);
        let min = px.iter().map(|p| luma(*p)).fold(1.0f32, f32::min);
        assert!(mean > 0.10, "backdrop too dark: mean luma {mean:.3}");
        assert!(mean < 0.75, "backdrop blown out: mean luma {mean:.3}");
        assert!(max > 0.65, "sky/ground lacks highlights: max luma {max:.3}");
        assert!(max - min > 0.15, "backdrop is flat: {min:.3}..{max:.3}");
    }

    /// The half-resolution soft-particle pass (opt-in) renders the same soft puff as the
    /// full-resolution path: both frames carry the puff, and the half-res pass tracks the
    /// full-res pass closely (a bilateral upsample, not a thinned copy). The flag is off by
    /// default (ADR-041), so the flag-off frame is the unchanged full-res path.
    #[test]
    fn half_res_soft_particles_render_and_track_full_res() {
        use crate::format::{
            BlendMode, ColorKey, EmitterDef, FloatKey, InitModule, RangeDef, RenderMode, RenderModule,
            ShapeDef, SpawnMode, SpawnModule, SpriteId, SystemModule, UpdateModule, VelocityDef,
            VelocityMode,
        };
        use crate::scene::{VfxLibrary, VfxScene};
        use crate::sim::LodInput;

        let device = match RenderDevice::shared_headless() {
            Ok(d) => d,
            Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
        };

        let puff = crate::VfxSystemDef {
            version: crate::format::VFX_FORMAT_VERSION,
            name: "puff".into(),
            system: SystemModule {
                bounds: None,
                budget: 256,
                lod: None,
            },
            emitters: vec![EmitterDef {
                name: "puff".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(32),
                    lifetime: RangeDef::from(30.0),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: RangeDef::from(0.0),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 1.0, 0.0),
                            spread: 10.0,
                            speed: RangeDef::from(1.0),
                        },
                        inherit: 0.0,
                    },
                    size: RangeDef::from(0.8),
                    rotation: RangeDef::from(0.0),
                    spin: RangeDef::from(0.0),
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#888888c8".into(),
                    }],
                },
                update: UpdateModule {
                    size_over_life: vec![FloatKey { t: 0.0, v: 0.6 }, FloatKey { t: 1.0, v: 2.0 }],
                    ..UpdateModule::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::Puff,
                    blend: BlendMode::Alpha,
                    softness: 0.5,
                    ..RenderModule::default()
                },
            }],
        };

        let render = |half_res: bool, spawn: bool| -> Vec<u8> {
            let mut profile = VfxProfile::desktop_high();
            profile.particle_capacity = 256;
            profile.soft_particle_res = if half_res {
                crate::gpu::SoftParticleRes::Half
            } else {
                crate::gpu::SoftParticleRes::Full
            };
            let mut harness = VfxHarness::new(&device, profile).expect("harness builds");
            let mut library = VfxLibrary::new();
            library.add(puff.clone()).expect("puff def");
            let mut scene = VfxScene::new(library);
            if spawn {
                scene
                    .spawn("puff", Vec3::new(0.0, 1.4, 0.0), Vec3::Z, 4)
                    .expect("spawn puff");
            }
            let camera = VfxCamera::looking_at(Vec3::new(0.0, 2.0, 7.0), Vec3::new(0.0, 1.2, 0.0));
            let lod = LodInput {
                camera_pos: camera.eye,
            };
            let mut pixels = Vec::new();
            for _ in 0..20 {
                scene.update(1.0 / 60.0, &lod, 256);
                scene.upload(harness.gpu());
                let frame = harness
                    .render(&device, &camera, 320, 180, 1.0 / 60.0)
                    .expect("half-res frame");
                pixels = frame.pixels;
            }
            pixels
        };
        // The no-puff backdrop, the default full-res path (flag off), and the half-res path
        // (flag on). The puff must be the difference from the backdrop in both paths, and the
        // half-res path must track the full-res path (a bilateral upsample, not a thinned copy).
        let backdrop = render(false, false);
        let full = render(false, true);
        let half = render(true, true);
        let changed = |a: &[u8], b: &[u8]| {
            let n = 320 * 180;
            (0..n)
                .filter(|&i| {
                    (0..3)
                        .map(|k| i32::from(a[i * 4 + k]).abs_diff(i32::from(b[i * 4 + k])))
                        .max()
                        .unwrap_or(0)
                        > 12
                })
                .count()
        };
        let full_px = changed(&full, &backdrop);
        let half_px = changed(&half, &backdrop);
        assert!(
            full_px > 40 && half_px > 40,
            "the puff did not render (full {full_px} px, half {half_px} px)"
        );
        // The half-res pass tracks the full-res pass: a depth-aware bilateral upsample of the
        // same soft puff, not a dropped or thinned copy.
        let n = 320 * 180;
        let mean_abs: f64 = (0..n)
            .map(|i| {
                (0..3)
                    .map(|k| (i32::from(full[i * 4 + k]) - i32::from(half[i * 4 + k])).abs() as f64)
                    .sum::<f64>()
                    / 3.0
            })
            .sum::<f64>()
            / n as f64;
        assert!(
            mean_abs < 6.0,
            "the half-res puff diverged from the full-res puff: mean |Δ| {mean_abs:.2}"
        );
    }
}
