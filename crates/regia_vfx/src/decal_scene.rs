// SPDX-License-Identifier: MPL-2.0
//! Offscreen set piece for projected decals: terrain, a normal-mapped mesh, and optional
//! receiver layers. The host forward renderer has no G-buffer, so this scene writes the two
//! views the material path needs (scene colour + receiver) and draws decals through [`VfxGpu`].
//!
//! Receiver texels are `Rgba8Unorm`: R = layer / 255, GBA = world normal * 0.5 + 0.5.
//! Lighting of the colour target uses the vertex normal, so a rippled receiver normal changes
//! only the decal relight.

use regia_core::math::Vec3;
use regia_render::RenderDevice;

use crate::decal::{Decal, DecalSurface};
use crate::gpu::{VfxCamera, VfxError, VfxGpu, VfxProfile};
use crate::offscreen::HARNESS_FORMAT;
use crate::timing::GpuTimer;

/// Sun direction shared with the M14 harness (`offscreen::SUN_DIR`), then normalised in the shader.
const SUN: [f32; 3] = [-0.45, 0.62, 0.42];

const SCENE_WGSL: &str = r#"
struct Uniforms {
    view_proj: mat4x4<f32>,
    sun: vec4<f32>,
    force_normal: vec4<f32>,
    params: vec4<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

struct Vert {
    @location(0) pos_layer: vec4<f32>,
    @location(1) nrm_ripple: vec4<f32>,
    @location(2) albedo: vec4<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) nrm: vec3<f32>,
    @location(2) albedo: vec3<f32>,
    @location(3) layer_ripple: vec2<f32>,
};

@vertex
fn vs_scene(v: Vert) -> VsOut {
    // A constructor, not a struct `var` (ADR-026).
    return VsOut(
        uniforms.view_proj * vec4<f32>(v.pos_layer.xyz, 1.0),
        v.pos_layer.xyz,
        v.nrm_ripple.xyz,
        v.albedo.rgb,
        vec2<f32>(v.pos_layer.w, v.nrm_ripple.w),
    );
}

struct FsOut {
    @location(0) color: vec4<f32>,
    @location(1) receiver: vec4<f32>,
};

@fragment
fn fs_scene(in: VsOut) -> FsOut {
    let n_geom = normalize(in.nrm);
    var n_rec = n_geom;
    if (in.layer_ripple.y > 0.5 && uniforms.params.x > 0.0) {
        n_rec.x = n_rec.x + sin(in.world.x * 6.0) * uniforms.params.x;
        n_rec.z = n_rec.z + cos(in.world.z * 6.0) * uniforms.params.x;
        n_rec = normalize(n_rec);
    }
    if (uniforms.force_normal.w > 0.5) {
        n_rec = normalize(uniforms.force_normal.xyz);
    }
    let ndl = clamp(dot(n_geom, normalize(uniforms.sun.xyz)), 0.0, 1.0);
    var albedo = in.albedo;
    if (uniforms.params.y > 0.5) {
        let ix = i32(floor(in.world.x));
        let iz = i32(floor(in.world.z));
        if (((ix + iz) & 1) == 0) {
            albedo = albedo * 0.82;
        }
    }
    let lit = albedo * (0.22 + 0.78 * ndl);
    let enc = n_rec * 0.5 + vec3<f32>(0.5);
    return FsOut(vec4<f32>(lit, 1.0), vec4<f32>(in.layer_ripple.x / 255.0, enc));
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SceneUniform {
    view_proj: [[f32; 4]; 4],
    sun: [f32; 4],
    force_normal: [f32; 4],
    params: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SceneVert {
    pos_layer: [f32; 4],
    nrm_ripple: [f32; 4],
    albedo: [f32; 4],
}

/// What one offscreen decal frame contains.
#[derive(Debug, Clone)]
pub struct DecalFrame {
    /// Viewport width.
    pub width: u32,
    /// Viewport height.
    pub height: u32,
    /// Camera shared by the backdrop and the decals.
    pub camera: VfxCamera,
    /// Decals drawn this frame (empty = backdrop only).
    pub decals: Vec<(Decal, DecalSurface)>,
    /// Large ground quad, receiver layer 0.
    pub ground: bool,
    /// Box mesh at (2.2, 0.5, 0), receiver layer 1. The top face can ripple.
    pub mesh: bool,
    /// Ripple amplitude written into the mesh-top receiver normal. Lighting stays flat.
    pub mesh_ripple: f32,
    /// 1 m checker on the ground albedo.
    pub checker: bool,
    /// Two horizontal quads (layer 0 at x = −0.8, layer 2 at x = 0.8) for the mask test.
    pub split_quads: bool,
    /// One horizontal quad at the origin, layer 0.
    pub single_quad: bool,
    /// When `w > 0.5`, every receiver normal is replaced with `xyz`.
    pub force_normal: [f32; 4],
}

impl Default for DecalFrame {
    fn default() -> Self {
        Self {
            width: 320,
            height: 180,
            camera: VfxCamera::looking_at(Vec3::new(0.0, 8.0, 10.0), Vec3::ZERO),
            decals: Vec::new(),
            ground: true,
            mesh: false,
            mesh_ripple: 0.0,
            checker: false,
            split_quads: false,
            single_quad: false,
            force_normal: [0.0; 4],
        }
    }
}

struct Targets {
    width: u32,
    height: u32,
    color: wgpu::Texture,
    color_view: wgpu::TextureView,
    receiver_view: wgpu::TextureView,
    scene_copy: wgpu::Texture,
    scene_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
    readback: wgpu::Buffer,
    padded: u32,
}

/// Backdrop + decal pass used by the analytical GPU tests and the evidence shots.
pub struct DecalScene {
    gpu: VfxGpu,
    uniform_buf: wgpu::Buffer,
    bg: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
    vb: wgpu::Buffer,
    ib: wgpu::Buffer,
    targets: Option<Targets>,
}

impl DecalScene {
    /// Installs the same library atlas used by the runtime renderer.
    pub fn set_decal_texture_atlas(
        &mut self,
        atlas: std::sync::Arc<crate::decal_texture::DecalTextureAtlas>,
    ) {
        self.gpu.set_decal_texture_atlas(atlas);
    }

    /// Creates the scene around a small [`VfxGpu`]. `profile.decal_capacity` is the budget.
    pub fn new(device: &RenderDevice, profile: VfxProfile) -> Result<Self, VfxError> {
        let d = &device.device;
        let gpu = VfxGpu::new(device, profile)?;
        let uniform_buf = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("decal scene uniform"),
            size: size_of::<SceneUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("decal scene bgl"),
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
            label: Some("decal scene bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });
        let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("decal scene"),
            source: wgpu::ShaderSource::Wgsl(SCENE_WGSL.into()),
        });
        let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("decal scene layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("decal scene pipe"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_scene"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<SceneVert>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 16,
                            shader_location: 1,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 32,
                            shader_location: 2,
                        },
                    ],
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_scene"),
                targets: &[
                    Some(wgpu::ColorTargetState {
                        format: HARNESS_FORMAT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    }),
                    Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    }),
                ],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
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
        let vb = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("decal scene vb"),
            size: (512 * size_of::<SceneVert>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let ib = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("decal scene ib"),
            size: (1024 * 4) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Self {
            gpu,
            uniform_buf,
            bg,
            pipeline,
            vb,
            ib,
            targets: None,
        })
    }

    /// Renders one frame and returns tightly packed sRGB RGBA8, top row first.
    pub fn render(&mut self, device: &RenderDevice, frame: &DecalFrame) -> Result<Vec<u8>, VfxError> {
        self.render_inner(device, frame, None)
    }

    /// Pixel size of the largest tile-union quad from the last [`DecalScene::render`].
    pub fn clip_extent(&self) -> (u32, u32) {
        self.gpu.clip_extent()
    }

    fn render_inner(
        &mut self,
        device: &RenderDevice,
        frame: &DecalFrame,
        timer: Option<&GpuTimer>,
    ) -> Result<Vec<u8>, VfxError> {
        let _span = tracing::info_span!("decal_scene").entered();
        let width = frame.width.max(1);
        let height = frame.height.max(1);
        crate::gpu::check_texture_extent(width, height, device.device.limits().max_texture_dimension_2d)?;
        self.ensure_targets(&device.device, width, height);
        let (verts, indices) = build_mesh(frame);
        if verts.is_empty() || indices.is_empty() {
            return Err(VfxError::Shader("decal scene has no geometry".into()));
        }
        if verts.len() > 512 || indices.len() > 1024 {
            return Err(VfxError::Shader(
                "decal scene mesh exceeds the staging buffers".into(),
            ));
        }
        let aspect = width as f32 / height as f32;
        let (_, _, view_proj) = frame.camera.matrices(aspect);
        let uniform = SceneUniform {
            view_proj: view_proj.to_cols_array_2d(),
            sun: [SUN[0], SUN[1], SUN[2], 0.0],
            force_normal: frame.force_normal,
            params: [frame.mesh_ripple, if frame.checker { 1.0 } else { 0.0 }, 0.0, 0.0],
        };
        device
            .queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniform));
        device
            .queue
            .write_buffer(&self.vb, 0, bytemuck::cast_slice(&verts));
        device
            .queue
            .write_buffer(&self.ib, 0, bytemuck::cast_slice(&indices));

        let targets = self
            .targets
            .as_ref()
            .ok_or_else(|| VfxError::Shader("targets missing".into()))?;
        let color_view = targets.color_view.clone();
        let receiver_view = targets.receiver_view.clone();
        let scene_view = targets.scene_view.clone();
        let depth_view = targets.depth_view.clone();
        let color_tex = targets.color.clone();
        let scene_tex = targets.scene_copy.clone();
        let readback = targets.readback.clone();
        let padded = targets.padded;

        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("decal scene"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("decal scene geometry"),
                color_attachments: &[
                    Some(wgpu::RenderPassColorAttachment {
                        view: &color_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.55,
                                g: 0.62,
                                b: 0.72,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    }),
                    Some(wgpu::RenderPassColorAttachment {
                        view: &receiver_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.5,
                                b: 1.0,
                                a: 0.5,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    }),
                ],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
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
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, Some(&self.bg), &[]);
            pass.set_vertex_buffer(0, self.vb.slice(..));
            pass.set_index_buffer(self.ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..indices.len() as u32, 0, 0..1);
        }
        encoder.copy_texture_to_texture(
            color_tex.as_image_copy(),
            scene_tex.as_image_copy(),
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        self.gpu
            .set_material_views(Some(&receiver_view), Some(&scene_view));
        self.gpu.set_depth_view(Some(&depth_view));
        let decals: Vec<Decal> = frame.decals.iter().map(|(d, _)| *d).collect();
        let surfaces: Vec<DecalSurface> = frame.decals.iter().map(|(_, s)| *s).collect();
        self.gpu.write_decals_with(&decals, &surfaces);
        self.gpu.set_frame(&frame.camera, 1.0 / 60.0, width, height);
        self.gpu.set_colour_format(HARNESS_FORMAT);
        {
            let _decal = tracing::info_span!("decal_pass").entered();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("decal scene decals"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: None,
                    stencil_ops: None,
                }),
                timestamp_writes: timer.map(|t| wgpu::RenderPassTimestampWrites {
                    query_set: t.query_set(),
                    beginning_of_pass_write_index: Some(0),
                    end_of_pass_write_index: Some(1),
                }),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.gpu.draw_decals(&mut pass, HARNESS_FORMAT)?;
        }
        encoder.copy_texture_to_buffer(
            color_tex.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
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
        if let Some(timer) = timer {
            timer.resolve(&mut encoder, 2);
        }
        device.queue.submit([encoder.finish()]);
        read_pixels(&device.device, &readback, width, height, padded)
    }

    fn ensure_targets(&mut self, d: &wgpu::Device, width: u32, height: u32) {
        if self
            .targets
            .as_ref()
            .is_some_and(|t| t.width == width && t.height == height)
        {
            return;
        }
        let color = texture(
            d,
            "decal scene color",
            width,
            height,
            HARNESS_FORMAT,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        );
        let receiver = texture(
            d,
            "decal scene receiver",
            width,
            height,
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let scene_copy = texture(
            d,
            "decal scene copy",
            width,
            height,
            HARNESS_FORMAT,
            wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let depth = texture(
            d,
            "decal scene depth",
            width,
            height,
            wgpu::TextureFormat::Depth32Float,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let padded =
            (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("decal scene readback"),
            size: padded as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.targets = Some(Targets {
            width,
            height,
            color_view: color.create_view(&Default::default()),
            // The view holds the GPU texture; the Rust handle is only needed for the copy sources.
            receiver_view: receiver.create_view(&Default::default()),
            scene_view: scene_copy.create_view(&Default::default()),
            depth_view: depth.create_view(&Default::default()),
            color,
            scene_copy,
            readback,
            padded,
        });
    }
}

/// Times one decal pass of `count` ground decals at `width`×`height`.
///
/// Returns `0` when the device has no timestamp queries (lavapipe often reports 0 anyway).
pub fn measure_decal_pass(
    device: &RenderDevice,
    count: u32,
    width: u32,
    height: u32,
) -> Result<f32, VfxError> {
    let profile = VfxProfile {
        particle_capacity: 64,
        decal_capacity: count.clamp(1, crate::gpu::MAX_DECALS),
        sort: false,
        light_budget: 0,
        screen_floor_px: 0.0,
        smoke_budget: 0,
        soft_particle_res: crate::gpu::SoftParticleRes::Full,
        event_capacity: 16,
        child_capacity: 16,
    };
    let mut scene = DecalScene::new(device, profile)?;
    let mut frame = DecalFrame {
        width,
        height,
        camera: VfxCamera::looking_at(Vec3::new(0.0, 18.0, 22.0), Vec3::ZERO),
        ground: true,
        checker: true,
        ..DecalFrame::default()
    };
    let side = (count as f32).sqrt().ceil().max(1.0);
    let mut decals = Vec::new();
    for i in 0..count {
        let x = (i as f32 % side) - side * 0.5;
        let z = (i as f32 / side).floor() - side * 0.5;
        let (decal, surface) = scorch_at(Vec3::new(x * 0.7, 0.02, z * 0.7));
        decals.push((decal, surface));
    }
    frame.decals = decals;
    let Some(timer) = GpuTimer::new(&device.device, 2) else {
        scene.render(device, &frame)?;
        return Ok(0.0);
    };
    scene.render_inner(device, &frame, Some(&timer))?;
    Ok(timer
        .read_ms(&device.device, &device.queue)
        .and_then(|ms| ms.first().copied())
        .unwrap_or(0.0))
}

/// Projects a world point to a pixel. `None` when it is behind the camera or off the viewport.
pub fn project_pixel(camera: &VfxCamera, width: u32, height: u32, world: Vec3) -> Option<(u32, u32)> {
    let aspect = width.max(1) as f32 / height.max(1) as f32;
    let (_, _, view_proj) = camera.matrices(aspect);
    let clip = view_proj * world.extend(1.0);
    if clip.w <= 1e-4 {
        return None;
    }
    let x = (clip.x / clip.w * 0.5 + 0.5) * width as f32;
    let y = (1.0 - (clip.y / clip.w * 0.5 + 0.5)) * height as f32;
    if x < 0.0 || y < 0.0 || x >= width as f32 || y >= height as f32 {
        return None;
    }
    Some((x as u32, y as u32))
}

/// A scorch prefab instance. Falls back to a hand-built surface if the example file fails to parse.
pub fn scorch_at(position: Vec3) -> (Decal, DecalSurface) {
    if let Some(prefab) = crate::decal_format::builtin_prefab("scorch")
        && let Ok(pair) = prefab.instantiate(position, Vec3::Y, 0.0, None)
    {
        return pair;
    }
    let decal = Decal {
        position,
        rotation: crate::decal::align_to_normal(Vec3::Y, 0.0),
        half_size: Vec3::new(1.4, 1.4, 0.35),
        color: [0.15, 0.05, 0.03, 1.0],
        age: 0.0,
        lifetime: None,
    };
    (decal, DecalSurface::from_decal(&decal))
}

fn texture(
    d: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    d.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

fn read_pixels(
    d: &wgpu::Device,
    buffer: &wgpu::Buffer,
    width: u32,
    height: u32,
    padded: u32,
) -> Result<Vec<u8>, VfxError> {
    let (tx, rx) = std::sync::mpsc::channel();
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    // Unmap on every path after the map was requested (a mapped buffer fails every later
    // `map_async`).
    let result = (|| -> Result<Vec<u8>, VfxError> {
        d.poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| VfxError::Shader(format!("decal readback poll: {err}")))?;
        rx.recv()
            .map_err(|_| VfxError::Shader("decal readback channel".into()))?
            .map_err(|err| VfxError::Shader(format!("decal readback map: {err}")))?;
        let mapped = slice
            .get_mapped_range()
            .map_err(|_| VfxError::Shader("decal readback range".into()))?;
        let row = (width * 4) as usize;
        let mut pixels = Vec::with_capacity(row * height as usize);
        for y in 0..height as usize {
            let start = y * padded as usize;
            pixels.extend_from_slice(&mapped[start..start + row]);
        }
        Ok(pixels)
    })();
    buffer.unmap();
    result
}

fn build_mesh(frame: &DecalFrame) -> (Vec<SceneVert>, Vec<u32>) {
    let mut verts = Vec::new();
    let mut indices = Vec::new();
    let ground_albedo = [0.42, 0.38, 0.30, 1.0];
    if frame.ground {
        quad(
            &mut verts,
            &mut indices,
            Quad {
                origin: Vec3::new(-12.0, 0.0, -12.0),
                edge_u: Vec3::new(24.0, 0.0, 0.0),
                edge_v: Vec3::new(0.0, 0.0, 24.0),
                normal: Vec3::Y,
                layer: 0.0,
                albedo: ground_albedo,
                ripple: 0.0,
            },
        );
    }
    if frame.mesh {
        push_box(
            &mut verts,
            &mut indices,
            Vec3::new(2.2, 0.5, 0.0),
            Vec3::new(0.8, 0.5, 0.8),
            1.0,
            [0.62, 0.58, 0.52, 1.0],
            frame.mesh_ripple > 0.0,
        );
    }
    if frame.single_quad {
        quad(
            &mut verts,
            &mut indices,
            Quad {
                origin: Vec3::new(-1.2, 0.05, -1.2),
                edge_u: Vec3::new(2.4, 0.0, 0.0),
                edge_v: Vec3::new(0.0, 0.0, 2.4),
                normal: Vec3::Y,
                layer: 0.0,
                albedo: [0.7, 0.68, 0.62, 1.0],
                ripple: 0.0,
            },
        );
    }
    if frame.split_quads {
        quad(
            &mut verts,
            &mut indices,
            Quad {
                origin: Vec3::new(-1.5, 0.05, -0.7),
                edge_u: Vec3::new(1.3, 0.0, 0.0),
                edge_v: Vec3::new(0.0, 0.0, 1.4),
                normal: Vec3::Y,
                layer: 0.0,
                albedo: [0.55, 0.5, 0.42, 1.0],
                ripple: 0.0,
            },
        );
        quad(
            &mut verts,
            &mut indices,
            Quad {
                origin: Vec3::new(0.2, 0.05, -0.7),
                edge_u: Vec3::new(1.3, 0.0, 0.0),
                edge_v: Vec3::new(0.0, 0.0, 1.4),
                normal: Vec3::Y,
                layer: 2.0,
                albedo: [0.55, 0.5, 0.42, 1.0],
                ripple: 0.0,
            },
        );
    }
    (verts, indices)
}

fn along(axis: Vec3, half: Vec3) -> f32 {
    half.dot(axis.abs())
}

fn push_box(
    verts: &mut Vec<SceneVert>,
    indices: &mut Vec<u32>,
    center: Vec3,
    half: Vec3,
    layer: f32,
    albedo: [f32; 4],
    ripple_top: bool,
) {
    let faces: [(Vec3, Vec3, Vec3, bool); 6] = [
        (Vec3::Y, Vec3::X, Vec3::Z, ripple_top),
        (Vec3::NEG_Y, Vec3::X, Vec3::NEG_Z, false),
        (Vec3::X, Vec3::NEG_Z, Vec3::Y, false),
        (Vec3::NEG_X, Vec3::Z, Vec3::Y, false),
        (Vec3::Z, Vec3::X, Vec3::Y, false),
        (Vec3::NEG_Z, Vec3::NEG_X, Vec3::Y, false),
    ];
    for (normal, axis_u, axis_v, ripple) in faces {
        let origin = center + normal * along(normal, half)
            - axis_u * along(axis_u, half)
            - axis_v * along(axis_v, half);
        let extent_u = axis_u * (along(axis_u, half) * 2.0);
        let extent_v = axis_v * (along(axis_v, half) * 2.0);
        quad(
            verts,
            indices,
            Quad {
                origin,
                edge_u: extent_u,
                edge_v: extent_v,
                normal,
                layer,
                albedo,
                ripple: if ripple { 1.0 } else { 0.0 },
            },
        );
    }
}

struct Quad {
    origin: Vec3,
    edge_u: Vec3,
    edge_v: Vec3,
    normal: Vec3,
    layer: f32,
    albedo: [f32; 4],
    ripple: f32,
}

fn quad(verts: &mut Vec<SceneVert>, indices: &mut Vec<u32>, face: Quad) {
    let base = verts.len() as u32;
    let n = face.normal.normalize_or(Vec3::Y);
    for (su, sv) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
        let p = face.origin + face.edge_u * su + face.edge_v * sv;
        verts.push(SceneVert {
            pos_layer: [p.x, p.y, p.z, face.layer],
            nrm_ripple: [n.x, n.y, n.z, face.ripple],
            albedo: face.albedo,
        });
    }
    indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

#[cfg(test)]
mod tests {
    /// Audit v034 WGSL M2: no struct or array in a function-local `var` (ADR-026), which Mesa's
    /// SPIR-V validation refuses; this shader is outside `cargo xtask check-shaders`.
    #[test]
    fn scene_shader_has_no_function_layout_errors() {
        let found =
            regia_render::wgsl_function_layout_errors("decal_scene.wgsl", super::SCENE_WGSL).expect("emits");
        assert!(found.is_empty(), "{found:?}");
    }
}
