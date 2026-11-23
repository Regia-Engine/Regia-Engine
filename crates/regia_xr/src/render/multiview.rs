// SPDX-License-Identifier: MPL-2.0
//! The multiview strategy (spec 58 § 2.6, ADR-1918): one render pass fills both layers of a
//! two-layer target with one directional light, ambient and the materials' factors.
//!
//! The renderer's frame is built around one camera and one set of screen-space targets, so running
//! it in one pass over two layers would mean a multiview variant of every pass. This is the
//! standalone headset's reduced set instead: it draws in its own pass and shader, and it takes the
//! per-layer strategy too, so spec 58 § 8.12 compares the same shading drawn both ways. The
//! full-quality path is the frame drawn twice ([`regia_render::FrameRenderer::render_eye`]).

use std::collections::HashMap;
use std::num::NonZeroU32;

use regia_core::glam::{Mat3, Mat4, Vec3};
use regia_render::{EyeCamera, FrameMesh, FrameScene, Frustum, LabVertex, exposure_from_ev100};

use crate::eye_targets::SIM_EYE_FORMAT;

const STEREO_UNIFORM: u64 = 224;
/// Bytes between object uniforms (the dynamic-offset alignment of the object buffer).
pub const OBJECT_SLOT: u64 = 256;
const OBJECT_UNIFORM: u64 = 176;
const SINGLE_UNIFORM: u64 = 16;
/// Appended when the device has multiview: `@builtin(view_index)` does not validate without it.
const MULTIVIEW_ENTRY: &str = "
@vertex
fn vs_multiview(in: VertexIn, @builtin(view_index) view: u32) -> VertexOut {
    return shade_vertex(in, view);
}
";

struct MeshEntry {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    count: u32,
    center: Vec3,
    radius: f32,
    vertex_bytes: usize,
    index_bytes: usize,
    /// The [`regia_render::mesh_content_revision`] these buffers hold.
    content_rev: u64,
}

/// Where a drawn mesh's buffers live for this frame.
enum Resident {
    /// A non-zero key in [`StereoForward`]'s cache.
    Cached(u64),
    /// A key-0 upload that dies with the frame.
    Fresh(usize),
}

struct Target {
    extent: (u32, u32),
    color: wgpu::Texture,
    depth: wgpu::Texture,
    color_array: wgpu::TextureView,
    depth_array: wgpu::TextureView,
    color_layers: [wgpu::TextureView; 2],
    depth_layers: [wgpu::TextureView; 2],
}

struct Gpu {
    stereo: wgpu::Buffer,
    singles: [wgpu::Buffer; 2],
    view_groups: [wgpu::BindGroup; 2],
    object_bgl: wgpu::BindGroupLayout,
    layered: wgpu::RenderPipeline,
    multiview: Option<wgpu::RenderPipeline>,
}

/// What [`StereoForward::render`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StereoForwardReport {
    /// Objects drawn after culling.
    pub drawn: u32,
    /// Render passes recorded (1 multiview, 2 per layer).
    pub passes: u32,
}

/// The reduced forward set.
///
/// A mesh with key 0 is uploaded for that frame and not kept. Any other key
/// keeps its buffers until [`Self::release_meshes`], or until the vertex or
/// index byte length changes. A same-size edit announced through
/// [`regia_render::note_mesh_content_changed`] rewrites the buffers in place
/// and refreshes the cull bounds (the same rule as the main mesh cache).
#[derive(Default)]
pub struct StereoForward {
    gpu: Option<Gpu>,
    target: Option<Target>,
    meshes: HashMap<u64, MeshEntry>,
    objects: Option<(wgpu::Buffer, u32, wgpu::BindGroup)>,
    object_scratch: Vec<u8>,
}

fn pipeline(
    d: &wgpu::Device,
    module: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    entry: &str,
    mask: Option<NonZeroU32>,
) -> wgpu::RenderPipeline {
    let vertex = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<LabVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &[
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: std::mem::offset_of!(LabVertex, position) as u64,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: std::mem::offset_of!(LabVertex, normal) as u64,
                shader_location: 1,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: std::mem::offset_of!(LabVertex, color) as u64,
                shader_location: 2,
            },
        ],
    };
    d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("xr stereo forward"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some(entry),
            buffers: &[Some(vertex)],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: SIM_EYE_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
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
        multiview_mask: mask,
        cache: None,
    })
}

fn uniform_entry(
    binding: u32,
    stage: wgpu::ShaderStages,
    dynamic: bool,
    size: u64,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: stage,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: dynamic,
            min_binding_size: wgpu::BufferSize::new(size),
        },
        count: None,
    }
}

fn build_gpu(d: &wgpu::Device) -> Gpu {
    let multiview_supported = d.features().contains(wgpu::Features::MULTIVIEW);
    let mut source = include_str!("../../shaders/xr_stereo_forward.wgsl").to_string();
    if multiview_supported {
        source.push_str(MULTIVIEW_ENTRY);
    }
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("xr stereo forward"),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    let view_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("xr stereo view"),
        entries: &[
            uniform_entry(0, wgpu::ShaderStages::VERTEX_FRAGMENT, false, STEREO_UNIFORM),
            uniform_entry(1, wgpu::ShaderStages::VERTEX, false, SINGLE_UNIFORM),
        ],
    });
    let object_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("xr stereo object"),
        entries: &[uniform_entry(
            0,
            wgpu::ShaderStages::VERTEX_FRAGMENT,
            true,
            OBJECT_UNIFORM,
        )],
    });
    let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("xr stereo forward"),
        bind_group_layouts: &[Some(&view_bgl), Some(&object_bgl)],
        immediate_size: 0,
    });
    let uniform = |label: &str, size: u64| {
        d.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    };
    let stereo = uniform("xr stereo", STEREO_UNIFORM);
    let singles = [
        uniform("xr stereo layer 0", SINGLE_UNIFORM),
        uniform("xr stereo layer 1", SINGLE_UNIFORM),
    ];
    let group = |single: &wgpu::Buffer| {
        d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("xr stereo view"),
            layout: &view_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: stereo.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: single.as_entire_binding(),
                },
            ],
        })
    };
    let view_groups = [group(&singles[0]), group(&singles[1])];
    let layered = pipeline(d, &module, &layout, "vs_single", None);
    let multiview =
        multiview_supported.then(|| pipeline(d, &module, &layout, "vs_multiview", NonZeroU32::new(0b11)));
    Gpu {
        stereo,
        singles,
        view_groups,
        object_bgl,
        layered,
        multiview,
    }
}

fn build_target(d: &wgpu::Device, extent: (u32, u32)) -> Target {
    let texture = |label: &str, format, usage| {
        d.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: extent.0,
                height: extent.1,
                depth_or_array_layers: 2,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    let color = texture(
        "xr stereo color",
        SIM_EYE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
    );
    let depth = texture(
        "xr stereo depth",
        wgpu::TextureFormat::Depth32Float,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
    );
    let array = |t: &wgpu::Texture| {
        t.create_view(&wgpu::TextureViewDescriptor {
            label: Some("xr stereo layers"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        })
    };
    let layer = |t: &wgpu::Texture, index: u32| {
        t.create_view(&wgpu::TextureViewDescriptor {
            label: Some("xr stereo layer"),
            dimension: Some(wgpu::TextureViewDimension::D2),
            base_array_layer: index,
            array_layer_count: Some(1),
            ..Default::default()
        })
    };
    Target {
        extent,
        color_array: array(&color),
        depth_array: array(&depth),
        color_layers: [layer(&color, 0), layer(&color, 1)],
        depth_layers: [layer(&depth, 0), layer(&depth, 1)],
        color,
        depth,
    }
}

fn copy_at(dst: &mut [u8], at: &mut usize, bytes: &[u8]) {
    let end = *at + bytes.len();
    dst[*at..end].copy_from_slice(bytes);
    *at = end;
}

/// Writes one object's 144-byte uniform at the start of `dst`. The caller zeros
/// the rest of the 256-byte slot, including the pad out to the 176-byte binding.
fn write_object(dst: &mut [u8], mesh: &FrameMesh<'_>) {
    let model = Mat4::from_cols_array_2d(&mesh.model);
    let normal = Mat3::from_mat4(model).inverse().transpose();
    let mut at = 0usize;
    copy_at(dst, &mut at, bytemuck::cast_slice(&model.to_cols_array()));
    // The rows of the normal matrix: the shader takes three dots.
    for row in [normal.row(0), normal.row(1), normal.row(2)] {
        copy_at(dst, &mut at, bytemuck::cast_slice(&[row.x, row.y, row.z, 0.0]));
    }
    let material = mesh.material;
    copy_at(dst, &mut at, bytemuck::cast_slice(&material.base_color));
    let [r, g, b] = material.emissive;
    let k = material.emissive_intensity;
    copy_at(
        dst,
        &mut at,
        bytemuck::cast_slice(&[r * k, g * k, b * k, material.roughness]),
    );
}

/// The bounding sphere (center, radius) of `vertices` in mesh space.
fn mesh_bounds(vertices: &[regia_render::LabVertex]) -> (Vec3, f32) {
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    for vertex in vertices {
        let p = Vec3::from_array(vertex.position);
        lo = lo.min(p);
        hi = hi.max(p);
    }
    ((lo + hi) * 0.5, (hi - lo).length() * 0.5)
}

fn upload_mesh(d: &wgpu::Device, q: &wgpu::Queue, mesh: &FrameMesh<'_>, content_rev: u64) -> MeshEntry {
    let upload = |label: &str, usage, bytes: &[u8]| {
        let buffer = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: bytes.len() as u64,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        q.write_buffer(&buffer, 0, bytes);
        buffer
    };
    let (center, radius) = mesh_bounds(mesh.vertices);
    let vertex_bytes = std::mem::size_of_val(mesh.vertices);
    let index_bytes = std::mem::size_of_val(mesh.indices);
    MeshEntry {
        vertices: upload(
            "xr stereo vertices",
            wgpu::BufferUsages::VERTEX,
            bytemuck::cast_slice(mesh.vertices),
        ),
        indices: upload(
            "xr stereo indices",
            wgpu::BufferUsages::INDEX,
            bytemuck::cast_slice(mesh.indices),
        ),
        count: mesh.indices.len() as u32,
        center,
        radius,
        vertex_bytes,
        index_bytes,
        content_rev,
    }
}

impl StereoForward {
    /// Whether `device` runs one pass over both layers.
    pub fn multiview_supported(device: &wgpu::Device) -> bool {
        device.features().contains(wgpu::Features::MULTIVIEW)
    }

    /// Drops cached meshes whose keys are in `keys`.
    ///
    /// The match is exact. A key this cache does not hold is ignored. Key 0 is
    /// never stored.
    pub fn release_meshes(&mut self, keys: &[u64]) {
        for key in keys {
            self.meshes.remove(key);
        }
    }

    /// Drops every cached mesh (the session ended; its keys may be released while no frame
    /// runs). The pipelines stay.
    pub(crate) fn clear_meshes(&mut self) {
        self.meshes.clear();
        crate::perf::set_mesh_entries(0);
    }

    /// Returns the cached bounds. Uploads when the key is new or the byte length changed, and
    /// rewrites the buffers in place when the mesh's content revision moved (same size).
    fn ensure_stable(
        meshes: &mut HashMap<u64, MeshEntry>,
        d: &wgpu::Device,
        q: &wgpu::Queue,
        key: u64,
        mesh: &FrameMesh<'_>,
    ) -> Option<(Vec3, f32)> {
        let vertex_bytes = std::mem::size_of_val(mesh.vertices);
        let index_bytes = std::mem::size_of_val(mesh.indices);
        let rev = regia_render::mesh_content_revision(key);
        let resident = meshes.get(&key);
        let size_changed = resident
            .is_none_or(|entry| entry.vertex_bytes != vertex_bytes || entry.index_bytes != index_bytes);
        let applied = resident.map_or(0, |entry| entry.content_rev);
        match regia_render::mesh_refresh(size_changed, rev, applied) {
            regia_render::MeshRefresh::Upload => {
                meshes.insert(key, upload_mesh(d, q, mesh, rev));
            }
            regia_render::MeshRefresh::Rewrite => {
                if let Some(entry) = meshes.get_mut(&key) {
                    q.write_buffer(&entry.vertices, 0, bytemuck::cast_slice(mesh.vertices));
                    q.write_buffer(&entry.indices, 0, bytemuck::cast_slice(mesh.indices));
                    (entry.center, entry.radius) = mesh_bounds(mesh.vertices);
                    entry.content_rev = rev;
                }
            }
            regia_render::MeshRefresh::Keep => {}
        }
        meshes.get(&key).map(|entry| (entry.center, entry.radius))
    }

    /// Draws `scene` for both eyes into a two-layer target and copies each layer to `images`:
    /// in one pass when `multiview` is set and the device can, else one pass per layer. Skinned
    /// meshes draw in their rest pose; blended materials draw opaque.
    pub fn render(
        &mut self,
        d: &wgpu::Device,
        q: &wgpu::Queue,
        scene: &FrameScene<'_>,
        cameras: &[EyeCamera; 2],
        images: &[wgpu::Texture; 2],
        multiview: bool,
    ) -> StereoForwardReport {
        let _span = tracing::trace_span!("xr_multiview", multiview).entered();
        let extent = (scene.width.max(1), scene.height.max(1));
        let gpu = self.gpu.get_or_insert_with(|| build_gpu(d));
        if self.target.as_ref().is_none_or(|t| t.extent != extent) {
            self.target = Some(build_target(d, extent));
        }
        let Some(target) = self.target.as_ref() else {
            return StereoForwardReport::default();
        };
        // Standard depth, so the frustum planes come from the shared cull matrix.
        let frustum = Frustum::from_view_proj(&cameras[0].cull);
        let mut fresh: Vec<MeshEntry> = Vec::new();
        let mut visible: Vec<(&FrameMesh<'_>, Resident)> = Vec::with_capacity(scene.meshes.len());
        for mesh in &scene.meshes {
            if mesh.vertices.is_empty() || mesh.indices.is_empty() {
                continue;
            }
            let (center, radius, resident) = if mesh.mesh_key == 0 {
                let entry = upload_mesh(d, q, mesh, 0);
                let center = entry.center;
                let radius = entry.radius;
                fresh.push(entry);
                (center, radius, Resident::Fresh(fresh.len() - 1))
            } else {
                let Some((center, radius)) = Self::ensure_stable(&mut self.meshes, d, q, mesh.mesh_key, mesh)
                else {
                    continue;
                };
                (center, radius, Resident::Cached(mesh.mesh_key))
            };
            let model = Mat4::from_cols_array_2d(&mesh.model);
            let world = model.transform_point3(center);
            let scale = model
                .x_axis
                .truncate()
                .length()
                .max(model.y_axis.truncate().length())
                .max(model.z_axis.truncate().length());
            if frustum.intersects_sphere(world.to_array(), radius * scale) {
                visible.push((mesh, resident));
            }
        }
        let capacity = visible.len().max(1) as u32;
        if self.objects.as_ref().is_none_or(|(_, have, _)| *have < capacity) {
            let capacity = capacity.next_power_of_two();
            let buffer = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("xr stereo objects"),
                size: OBJECT_SLOT * u64::from(capacity),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("xr stereo objects"),
                layout: &gpu.object_bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &buffer,
                        offset: 0,
                        size: wgpu::BufferSize::new(OBJECT_UNIFORM),
                    }),
                }],
            });
            self.objects = Some((buffer, capacity, group));
        }
        let stride = OBJECT_SLOT as usize;
        let total = visible.len() * stride;
        self.object_scratch.resize(total, 0);
        self.object_scratch.fill(0);
        for (slot, (mesh, _)) in visible.iter().enumerate() {
            write_object(&mut self.object_scratch[slot * stride..], mesh);
        }
        let Some((objects, _, object_group)) = self.objects.as_ref() else {
            return StereoForwardReport::default();
        };
        if total > 0 {
            q.write_buffer(objects, 0, &self.object_scratch);
            crate::perf::object_uniform_write(total as u64);
        }
        let toward = -Vec3::from_array(scene.directional.direction).normalize_or_zero();
        let over_pi = std::f32::consts::FRAC_1_PI;
        let sun = Vec3::from_array(scene.directional.color) * scene.directional.illuminance * over_pi;
        let ambient = scene.ambient_lux * over_pi;
        let mut stereo = Vec::with_capacity(STEREO_UNIFORM as usize);
        for camera in cameras {
            let view_proj = camera.projection * camera.view;
            stereo.extend_from_slice(bytemuck::cast_slice(&view_proj.to_cols_array()));
        }
        for camera in cameras {
            let eye = camera.position();
            stereo.extend_from_slice(bytemuck::cast_slice(&[eye.x, eye.y, eye.z, 0.0]));
        }
        let exposure = exposure_from_ev100(scene.exposure_ev);
        stereo.extend_from_slice(bytemuck::cast_slice(&[toward.x, toward.y, toward.z, exposure]));
        stereo.extend_from_slice(bytemuck::cast_slice(&[sun.x, sun.y, sun.z, 0.0]));
        stereo.extend_from_slice(bytemuck::cast_slice(&[ambient, ambient, ambient, 0.0]));
        q.write_buffer(&gpu.stereo, 0, &stereo);
        for (index, single) in gpu.singles.iter().enumerate() {
            q.write_buffer(single, 0, bytemuck::bytes_of(&[index as u32, 0, 0, 0]));
        }
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("xr stereo forward"),
        });
        let clear = wgpu::Color {
            r: 0.02,
            g: 0.02,
            b: 0.025,
            a: 1.0,
        };
        let record =
            |pass: &mut wgpu::RenderPass<'_>, pipe: &wgpu::RenderPipeline, group: &wgpu::BindGroup| {
                pass.set_pipeline(pipe);
                pass.set_bind_group(0, group, &[]);
                for (slot, (_, resident)) in visible.iter().enumerate() {
                    let Some(entry) = (match resident {
                        Resident::Cached(key) => self.meshes.get(key),
                        Resident::Fresh(index) => fresh.get(*index),
                    }) else {
                        continue;
                    };
                    pass.set_bind_group(1, object_group, &[(slot as u64 * OBJECT_SLOT) as u32]);
                    pass.set_vertex_buffer(0, entry.vertices.slice(..));
                    pass.set_index_buffer(entry.indices.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..entry.count, 0, 0..1);
                }
            };
        let begin = |encoder: &'_ mut wgpu::CommandEncoder,
                     color: &wgpu::TextureView,
                     depth: &wgpu::TextureView,
                     mask: Option<NonZeroU32>| {
            encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("xr stereo forward"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: color,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(clear),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: depth,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Clear(1.0),
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: mask,
                })
                .forget_lifetime()
        };
        let passes = match gpu.multiview.as_ref().filter(|_| multiview) {
            Some(pipe) => {
                let mut pass = begin(
                    &mut encoder,
                    &target.color_array,
                    &target.depth_array,
                    NonZeroU32::new(0b11),
                );
                record(&mut pass, pipe, &gpu.view_groups[0]);
                1
            }
            None => {
                for layer in 0..2 {
                    let mut pass = begin(
                        &mut encoder,
                        &target.color_layers[layer],
                        &target.depth_layers[layer],
                        None,
                    );
                    record(&mut pass, &gpu.layered, &gpu.view_groups[layer]);
                }
                2
            }
        };
        for (layer, image) in images.iter().enumerate() {
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &target.color,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: layer as u32,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                image.as_image_copy(),
                wgpu::Extent3d {
                    width: extent.0,
                    height: extent.1,
                    depth_or_array_layers: 1,
                },
            );
        }
        q.submit([encoder.finish()]);
        crate::perf::set_mesh_entries(self.meshes.len() as u64);
        StereoForwardReport {
            drawn: visible.len() as u32,
            passes,
        }
    }

    /// The two-layer depth target (tests).
    pub fn depth_texture(&self) -> Option<&wgpu::Texture> {
        self.target.as_ref().map(|t| &t.depth)
    }
}
