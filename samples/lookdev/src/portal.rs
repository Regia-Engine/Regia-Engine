// SPDX-License-Identifier: MIT-0
//! Draws [`regia_terrain::MeshTunnel::render_mesh`] into the look-dev frame.
//!
//! The terrain hook punches the hole and does not submit the mesh. This pass uses the
//! same view-projection and the same EV100 / AgX lighting as the heightfield, with
//! back-face culling off so the interior reads from the road.

use regia_terrain::MeshTunnel;
use regia_terrain::gpu::{TerrainCamera, TimeOfDay};

use crate::LookdevError;
use crate::shots::ShotSun;

/// Rock-dark albedo. Lit rims stay readable; faces away from the sun fall off so the
/// opening reads as a mouth rather than another patch of snow.
const PORTAL_ALBEDO: [f32; 3] = [0.30, 0.28, 0.26];
const UNIFORM_BYTES: u64 = 144;

/// GPU draw of the tunnel mouth. The terrain hook only punches the hole.
pub(crate) struct PortalDraw {
    pipeline: wgpu::RenderPipeline,
    bind: wgpu::BindGroup,
    uniform: wgpu::Buffer,
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

impl PortalDraw {
    /// Uploads `tunnel`'s render mesh and compiles the portal pipeline.
    ///
    /// # Errors
    ///
    /// [`LookdevError::Config`] when the mesh attributes or indices are unusable.
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        tunnel: &MeshTunnel,
    ) -> Result<Self, LookdevError> {
        let mesh = &tunnel.render_mesh;
        if mesh.positions.len() != mesh.normals.len() || mesh.positions.is_empty() {
            return Err(LookdevError::Config(format!(
                "portal mesh has {} positions and {} normals",
                mesh.positions.len(),
                mesh.normals.len()
            )));
        }
        if mesh.indices.is_empty() || !mesh.indices.len().is_multiple_of(3) {
            return Err(LookdevError::Config(format!(
                "portal mesh index count {} is not a multiple of 3",
                mesh.indices.len()
            )));
        }
        let vert_count = mesh.positions.len() as u32;
        if mesh.indices.iter().any(|index| *index >= vert_count) {
            return Err(LookdevError::Config("portal mesh index out of range".into()));
        }

        let mut vertex_bytes = Vec::with_capacity(mesh.positions.len() * 24);
        for (position, normal) in mesh.positions.iter().zip(mesh.normals.iter()) {
            for value in position.iter().chain(normal.iter()) {
                vertex_bytes.extend(value.to_le_bytes());
            }
        }
        let mut index_bytes = Vec::with_capacity(mesh.indices.len() * 4);
        for index in &mesh.indices {
            index_bytes.extend(index.to_le_bytes());
        }

        let vertices = upload(
            device,
            queue,
            "lookdev portal vertices",
            wgpu::BufferUsages::VERTEX,
            &vertex_bytes,
        );
        let indices = upload(
            device,
            queue,
            "lookdev portal indices",
            wgpu::BufferUsages::INDEX,
            &index_bytes,
        );
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lookdev portal uniform"),
            size: UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lookdev portal layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX.union(wgpu::ShaderStages::FRAGMENT),
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lookdev portal bind"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lookdev portal"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/portal.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("lookdev portal pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("lookdev portal"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: 24,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x3,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x3,
                            offset: 12,
                            shader_location: 1,
                        },
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                front_face: wgpu::FrontFace::Ccw,
                // The collision box's opening faces the road or the interior depending on
                // winding. Either way the mouth has to be visible from outside.
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });

        tracing::info!(
            target: "lookdev",
            triangles = mesh.indices.len() / 3,
            "portal mesh uploaded"
        );

        Ok(Self {
            pipeline,
            bind,
            uniform,
            vertices,
            indices,
            index_count: mesh.indices.len() as u32,
        })
    }

    /// Writes this frame's camera and sun into the portal uniform.
    pub(crate) fn write_frame(&self, queue: &wgpu::Queue, camera: &TerrainCamera, sun: &ShotSun) {
        let tod = TimeOfDay {
            azimuth_radians: sun.azimuth,
            elevation_radians: sun.elevation,
            sun_color: sun.color,
            illuminance_lux: sun.lux,
            ambient: sun.ambient,
        };
        let direction = tod.sun_direction();
        // Same sunny-day exposure as `TerrainGpu::update_frame`.
        let exposure = 1.0 / (1.2 * 14.5_f32.exp2());
        let mut bytes = Vec::with_capacity(UNIFORM_BYTES as usize);
        push_mat(&mut bytes, &camera.view_projection());
        push_f32s(
            &mut bytes,
            &[camera.position[0], camera.position[1], camera.position[2], 0.0],
        );
        push_f32s(
            &mut bytes,
            &[direction[0], direction[1], direction[2], sun.lux * exposure],
        );
        push_f32s(&mut bytes, &[sun.color[0], sun.color[1], sun.color[2], 0.0]);
        push_f32s(&mut bytes, &[sun.ambient[0], sun.ambient[1], sun.ambient[2], 0.0]);
        push_f32s(
            &mut bytes,
            &[PORTAL_ALBEDO[0], PORTAL_ALBEDO[1], PORTAL_ALBEDO[2], 1.0],
        );
        debug_assert_eq!(bytes.len() as u64, UNIFORM_BYTES);
        queue.write_buffer(&self.uniform, 0, &bytes);
    }

    /// Draws the mesh. The caller has already cleared depth in this pass.
    pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.index_count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind, &[]);
        pass.set_vertex_buffer(0, self.vertices.slice(..));
        pass.set_index_buffer(self.indices.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..self.index_count, 0, 0..1);
    }
}

fn upload(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    usage: wgpu::BufferUsages,
    bytes: &[u8],
) -> wgpu::Buffer {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes.len() as u64,
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&buffer, 0, bytes);
    buffer
}

fn push_f32s(out: &mut Vec<u8>, values: &[f32]) {
    for value in values {
        out.extend(value.to_le_bytes());
    }
}

fn push_mat(out: &mut Vec<u8>, matrix: &[[f32; 4]; 4]) {
    for column in matrix {
        push_f32s(out, column);
    }
}
