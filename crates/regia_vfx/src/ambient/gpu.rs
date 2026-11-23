// SPDX-License-Identifier: MPL-2.0
//! The ambient-life draw: six pipelines per color format (built lazily or in
//! [`AmbientLifeGpu::warm`]), one uniform buffer and the scene depth. Instanced vertex work only.

use std::collections::HashMap;

use regia_core::math::Vec3;
use regia_render::{PrecipOcclusionBinding, RenderDevice};

use super::{AMBIENT_LOOP_S, AmbientBox, AmbientCounts, AmbientFrame, BirdInstance, MAX_BIRDS};
use crate::gpu::{VfxCamera, VfxError, VfxLighting, VfxProfile};
use crate::precip::PrecipTarget;
use crate::quality::EXPOSURE_FLOOR;

const SHADER: &str = include_str!("ambient.wgsl");

/// The shader text: the box and loop constants from Rust, then `ambient.wgsl`.
fn shader_text() -> String {
    let row = |name: &str, b: AmbientBox| {
        format!(
            "const {name}: vec3<f32> = vec3<f32>({:?}, {:?}, {:?});\n",
            b.half_extent_m, b.half_height_m, b.center_y_m
        )
    };
    format!(
        "const LOOP_S: f32 = {:?};\n{}{}{}{}{}{SHADER}",
        AMBIENT_LOOP_S as f32,
        row("MOTE_BOX", AmbientBox::MOTES),
        row("SWARM_BOX", AmbientBox::SWARMS),
        row("BUTTERFLY_BOX", AmbientBox::BUTTERFLIES),
        row("FIREFLY_BOX", AmbientBox::FIREFLIES),
        row("LEAF_BOX", AmbientBox::LEAVES),
    )
}

/// The WGSL `cargo xtask check-shaders` validates.
#[must_use]
pub fn ambient_shader_source() -> (String, String) {
    ("vfx ambient life".to_string(), shader_text())
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct AmbientUniform {
    view_proj: [[f32; 4]; 4],
    right: [f32; 4],
    up: [f32; 4],
    forward: [f32; 4],
    sun_dir: [f32; 4],
    sun_color: [f32; 4],
    sky_up: [f32; 4],
    sky_down: [f32; 4],
    target_info: [f32; 4],
    counts_a: [f32; 4],
    counts_b: [f32; 4],
    mote_drift: [f32; 4],
    leaf_fall: [f32; 4],
    leaf_drift: [f32; 4],
    eye_mod: [[f32; 4]; 5],
    seed: [u32; 4],
    eye: [f32; 4],
    occ_view_proj: [[f32; 4]; 4],
    occ_rect: [f32; 4],
    occ_info: [f32; 4],
    birds: [BirdInstance; MAX_BIRDS],
}

/// The Rust and WGSL struct row of the ambient-life uniform (`shader_contract`, ADR-3002;
/// ADR-5960 (11)). The shader's five `eye_mod_*` members are the five rows of `eye_mod`.
pub(crate) fn contract_rows() -> Vec<regia_render::ContractRow> {
    use std::mem::offset_of;
    let eye_mod = offset_of!(AmbientUniform, eye_mod);
    vec![regia_render::ContractRow::new(
        "vfx ambient life",
        "Ambient",
        std::mem::size_of::<AmbientUniform>(),
        vec![
            ("view_proj", offset_of!(AmbientUniform, view_proj)),
            ("right", offset_of!(AmbientUniform, right)),
            ("up", offset_of!(AmbientUniform, up)),
            ("forward", offset_of!(AmbientUniform, forward)),
            ("sun_dir", offset_of!(AmbientUniform, sun_dir)),
            ("sun_color", offset_of!(AmbientUniform, sun_color)),
            ("sky_up", offset_of!(AmbientUniform, sky_up)),
            ("sky_down", offset_of!(AmbientUniform, sky_down)),
            ("target_info", offset_of!(AmbientUniform, target_info)),
            ("counts_a", offset_of!(AmbientUniform, counts_a)),
            ("counts_b", offset_of!(AmbientUniform, counts_b)),
            ("mote_drift", offset_of!(AmbientUniform, mote_drift)),
            ("leaf_fall", offset_of!(AmbientUniform, leaf_fall)),
            ("leaf_drift", offset_of!(AmbientUniform, leaf_drift)),
            ("eye_mod_motes", eye_mod),
            ("eye_mod_swarms", eye_mod + 16),
            ("eye_mod_butterflies", eye_mod + 32),
            ("eye_mod_fireflies", eye_mod + 48),
            ("eye_mod_leaves", eye_mod + 64),
            ("seed", offset_of!(AmbientUniform, seed)),
            ("eye", offset_of!(AmbientUniform, eye)),
            ("occ_view_proj", offset_of!(AmbientUniform, occ_view_proj)),
            ("occ_rect", offset_of!(AmbientUniform, occ_rect)),
            ("occ_info", offset_of!(AmbientUniform, occ_info)),
            ("birds", offset_of!(AmbientUniform, birds)),
        ],
    )]
}

/// The camera, lighting and target of one drawn frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbientView {
    /// The camera.
    pub camera: VfxCamera,
    /// The sun and the exposure.
    pub lighting: VfxLighting,
    /// The target (the same shape the precipitation takes).
    pub target: PrecipTarget,
}

/// The bound occlusion view as the uniform takes it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SunView {
    view_proj: [[f32; 4]; 4],
    uv_rect: [f32; 4],
    /// Atlas edge, tile edge, 1 (bound), unused.
    info: [f32; 4],
}

/// The six pipelines of one color format.
struct Pipes {
    motes: wgpu::RenderPipeline,
    swarms: wgpu::RenderPipeline,
    fireflies: wgpu::RenderPipeline,
    butterflies: wgpu::RenderPipeline,
    leaves: wgpu::RenderPipeline,
    birds: wgpu::RenderPipeline,
}

/// Instances of each kind the next draw records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AmbientInstances {
    /// Motes.
    pub motes: u32,
    /// Swarm members.
    pub swarm_members: u32,
    /// Butterflies.
    pub butterflies: u32,
    /// Fireflies.
    pub fireflies: u32,
    /// Leaves.
    pub leaves: u32,
    /// Birds.
    pub birds: u32,
}

impl AmbientInstances {
    /// `true` when nothing is drawn.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The GPU side of ambient life.
pub struct AmbientLifeGpu {
    device: wgpu::Device,
    module: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
    bgl: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
    scene_depth: Option<wgpu::TextureView>,
    dummy_depth: wgpu::TextureView,
    occ_atlas: Option<wgpu::TextureView>,
    /// The sun view's matrix, tile and atlas, `None` unbound.
    occlusion: Option<SunView>,
    bind_group: Option<wgpu::BindGroup>,
    pipes: HashMap<wgpu::TextureFormat, Pipes>,
    cpu: Box<AmbientUniform>,
    instances: AmbientInstances,
}

impl AmbientLifeGpu {
    /// Creates the uniform and the shader. Pipelines build on first use, or in [`Self::warm`].
    ///
    /// # Errors
    /// Never today; keeps the shape of `VfxGpu::new`.
    pub fn new(device: &RenderDevice, _profile: &VfxProfile) -> Result<Self, VfxError> {
        let d = &device.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vfx ambient life"),
            source: wgpu::ShaderSource::Wgsl(shader_text().into()),
        });
        let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vfx ambient life bgl"),
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
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vfx ambient life layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let uniform = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vfx ambient life uniform"),
            size: size_of::<AmbientUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let dummy = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("vfx ambient life dummy depth"),
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
            scene_depth: None,
            dummy_depth: dummy.create_view(&wgpu::TextureViewDescriptor::default()),
            occ_atlas: None,
            occlusion: None,
            bind_group: None,
            pipes: HashMap::new(),
            cpu: Box::new(bytemuck::Zeroable::zeroed()),
            instances: AmbientInstances::default(),
        })
    }

    fn build_pipes(&self, format: wgpu::TextureFormat) -> Pipes {
        // Premultiplied: covering creatures write alpha, glowing ones (alpha 0) only add.
        let premult = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
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
                        depth_compare: Some(wgpu::CompareFunction::LessEqual),
                        stencil: wgpu::StencilState::default(),
                        bias: wgpu::DepthBiasState::default(),
                    }),
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: regia_render::pipeline_cache::cache_for(&self.device).as_deref(),
                })
        };
        Pipes {
            motes: build("vfx ambient motes", "vs_mote", "fs_dot"),
            swarms: build("vfx ambient swarms", "vs_swarm", "fs_dot"),
            fireflies: build("vfx ambient fireflies", "vs_firefly", "fs_glow"),
            butterflies: build("vfx ambient butterflies", "vs_butterfly", "fs_butterfly"),
            leaves: build("vfx ambient leaves", "vs_leaf", "fs_leaf"),
            birds: build("vfx ambient birds", "vs_bird", "fs_wing"),
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

    /// Color formats with built pipelines.
    #[must_use]
    pub fn warmed_formats(&self) -> usize {
        self.pipes.len()
    }

    /// Binds the frame's scene depth (the soft fade), keyed by the view's id.
    pub fn bind_depth(&mut self, depth: &wgpu::TextureView) {
        if self.scene_depth.as_ref() != Some(depth) {
            self.scene_depth = Some(depth.clone());
            self.bind_group = None;
        }
    }

    /// Binds the sun's occlusion view (the precipitation's view aimed along the sun); `None` is
    /// sunlight everywhere: motes and insect wings in its shade do not glint.
    pub fn bind_occlusion(&mut self, occlusion: Option<&PrecipOcclusionBinding<'_>>) {
        match occlusion {
            Some(binding) => {
                if self.occ_atlas.as_ref() != Some(binding.atlas) {
                    self.occ_atlas = Some(binding.atlas.clone());
                    self.bind_group = None;
                }
                self.occlusion = Some(SunView {
                    view_proj: binding.view_proj,
                    uv_rect: binding.uv_rect,
                    info: [binding.atlas_texels, binding.tile_texels, 1.0, 0.0],
                });
            }
            None => self.occlusion = None,
        }
    }

    /// Uploads the frame: one uniform write. A frame with nothing to draw uploads nothing.
    pub fn prepare(&mut self, queue: &wgpu::Queue, frame: &AmbientFrame, view: &AmbientView) {
        let counts = frame.counts;
        let birds = frame.bird_count.min(MAX_BIRDS as u32);
        self.instances = AmbientInstances {
            motes: AmbientCounts::instances(counts.motes),
            swarm_members: AmbientCounts::instances(counts.swarm_members),
            butterflies: AmbientCounts::instances(counts.butterflies),
            fireflies: AmbientCounts::instances(counts.fireflies),
            leaves: AmbientCounts::instances(counts.leaves),
            birds,
        };
        if self.instances.is_empty() {
            return;
        }
        let camera = view.camera;
        let target = view.target;
        let aspect = target.width.max(1) as f32 / target.height.max(1) as f32;
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
        let px_per_m = target.height.max(1) as f32 * 0.5 / (fov_y * 0.5).tan().max(1.0e-6);
        let light = view.lighting;
        let sun = light.sun_direction;
        let cloud = sun_through_cloud(sun[1], target.cloud_coverage);
        let padded = |v: [f32; 3], w: f32| [v[0], v[1], v[2], w];
        let u = &mut *self.cpu;
        u.view_proj = view_proj.to_cols_array_2d();
        u.right = padded(right.to_array(), near);
        u.up = padded(up.to_array(), far);
        u.forward = padded(forward.to_array(), px_per_m);
        u.sun_dir = padded(sun, cloud);
        u.sun_color = padded(light.sun_color, light.ambient);
        u.sky_up = target.sky_rows[0];
        u.sky_down = target.sky_rows[1];
        u.target_info = [
            light.exposure.max(EXPOSURE_FLOOR),
            target.target_mode,
            frame.clock_s,
            frame.season.index() as f32,
        ];
        u.counts_a = [
            counts.motes,
            counts.swarm_members,
            counts.butterflies,
            counts.fireflies,
        ];
        u.counts_b = [counts.leaves, birds as f32, 0.0, 0.0];
        u.mote_drift = padded(frame.mote_drift_m, 0.0);
        u.leaf_fall = frame.leaf_fall_m;
        u.leaf_drift = [frame.leaf_drift_m[0], frame.leaf_drift_m[1], 0.0, 0.0];
        for (row, eye_mod) in u.eye_mod.iter_mut().zip(frame.eye_mod_m) {
            *row = padded(eye_mod, 0.0);
        }
        u.seed = [frame.seed, 0, 0, 0];
        u.eye = [frame.eye[0] as f32, frame.eye[1] as f32, frame.eye[2] as f32, 0.0];
        match self.occlusion {
            Some(view) => {
                u.occ_view_proj = view.view_proj;
                u.occ_rect = view.uv_rect;
                u.occ_info = view.info;
            }
            None => u.occ_info = [1.0, 1.0, 0.0, 0.0],
        }
        u.birds = frame.birds;
        queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&*self.cpu));
    }

    fn bind_group(&mut self) -> Option<wgpu::BindGroup> {
        if self.bind_group.is_none() {
            let depth = self.scene_depth.clone()?;
            let atlas = self.occ_atlas.clone().unwrap_or_else(|| self.dummy_depth.clone());
            self.bind_group = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vfx ambient life bg"),
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

    /// Instances of each kind the next draw records.
    #[must_use]
    pub fn instances(&self) -> AmbientInstances {
        self.instances
    }

    /// Draws what [`Self::prepare`] uploaded into `pass`. Records nothing when nothing lives.
    pub fn draw(&mut self, pass: &mut wgpu::RenderPass<'_>, format: wgpu::TextureFormat) {
        if self.instances.is_empty() {
            return;
        }
        let _span = tracing::trace_span!("vfx_ambient_life").entered();
        if !self.pipes.contains_key(&format) {
            self.warm(&[format]);
        }
        let Some(bind_group) = self.bind_group() else {
            return;
        };
        let Some(pipes) = self.pipes.get(&format) else {
            return;
        };
        pass.set_bind_group(0, Some(&bind_group), &[]);
        let n = self.instances;
        // Covering creatures first, then the light that adds over them.
        let draws = [
            (&pipes.leaves, 6, n.leaves),
            (&pipes.butterflies, 12, n.butterflies),
            (&pipes.birds, 18, n.birds),
            (&pipes.swarms, 6, n.swarm_members),
            (&pipes.motes, 6, n.motes),
            (&pipes.fireflies, 6, n.fireflies),
        ];
        for (pipeline, vertices, instances) in draws {
            if instances > 0 {
                pass.set_pipeline(pipeline);
                pass.draw(0..vertices, 0..instances);
            }
        }
    }
}

/// How much of the sun reaches a creature, `0..1`: through the cloud, and gone before the sun's on
/// the horizon. The renderer has no view along a sun under about 1.15° (a sine of 0.02), and without
/// that view nothing is in the shade, so the light has to be out by then: otherwise every mote in a
/// dark forest would light up in one frame just before sunset.
pub(super) fn sun_through_cloud(sun_elevation_sin: f32, cloud_coverage: f32) -> f32 {
    let sun_up = super::smoothstep(0.02, 0.07, sun_elevation_sin);
    (1.0 - 0.8 * cloud_coverage.clamp(0.0, 1.0)) * sun_up
}
