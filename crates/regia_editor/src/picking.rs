// SPDX-License-Identifier: MPL-2.0
//! GPU picking: an entity-id buffer (`docs/specs/19-editor.md` § 3).
//!
//! The pass draws every mesh the viewport rendered — same meshes, same world matrices, same
//! camera — into an `R32Uint` id target and an `R32Float` eye-distance target with a depth
//! buffer, then reads both back. A click reads one pixel (pixel-exact on the rendered
//! geometry), a box select collects the ids visible inside a rectangle, and the selection
//! outline is the boundary of the selected ids in the same buffer. Ids index a per-frame
//! table of entities (0 = nothing); part entities map back to their owner
//! ([`crate::mesh_source::pick_owner`]).
//!
//! The terrain is not drawn here (it renders through the terrain's own shared pass); the
//! viewport ray-marches its heightfield on the CPU ([`pick_terrain`]) and keeps whichever hit
//! is nearer, using the distance target. Skinned meshes are drawn in bind pose.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use regia_core::math::{Mat4, Vec3};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::World;
use regia_render::{CpuMesh, RenderDevice};

/// One mesh instance for the id pass.
#[derive(Clone)]
pub struct IdDraw {
    /// Shared geometry (GPU upload cached by the `Arc`).
    pub mesh: Arc<CpuMesh>,
    /// Object → world.
    pub model: Mat4,
    /// The entity this instance selects.
    pub entity: Entity,
}

/// Orders `draws` by entity id (stable within an entity) so equal-depth surfaces resolve the
/// same way whatever order the world's archetypes happen to iterate in.
pub fn sort_id_draws(draws: &mut [IdDraw]) {
    draws.sort_by_key(|draw| draw.entity.to_bits());
}

/// A read-back id buffer.
#[derive(Debug, Clone, Default)]
pub struct IdBuffer {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row-major ids (0 = background; `n` = `entities[n - 1]`).
    pub ids: Vec<u32>,
    /// Row-major eye distance in metres (`f32::INFINITY` on background).
    pub distance: Vec<f32>,
    /// The entity table for this frame.
    pub entities: Vec<Entity>,
}

impl IdBuffer {
    /// The entity at a pixel and its eye distance (`None` on background or outside).
    #[must_use]
    pub fn at(&self, x: u32, y: u32) -> Option<(Entity, f32)> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = (y * self.width + x) as usize;
        let id = *self.ids.get(index)?;
        let entity = *self.entities.get((id as usize).checked_sub(1)?)?;
        Some((entity, self.distance.get(index).copied().unwrap_or(f32::INFINITY)))
    }

    /// Every entity with at least one visible pixel inside the rectangle (inclusive corners,
    /// any order), sorted by entity bits.
    #[must_use]
    pub fn entities_in_rect(&self, a: (u32, u32), b: (u32, u32)) -> Vec<Entity> {
        if self.width == 0 || self.height == 0 {
            return Vec::new();
        }
        let x0 = a.0.min(b.0).min(self.width - 1);
        let x1 = a.0.max(b.0).min(self.width - 1);
        let y0 = a.1.min(b.1).min(self.height - 1);
        let y1 = a.1.max(b.1).min(self.height - 1);
        let mut ids = BTreeSet::new();
        for y in y0..=y1 {
            let row = (y * self.width) as usize;
            for x in x0..=x1 {
                let id = self.ids[row + x as usize];
                if id != 0 {
                    ids.insert(id);
                }
            }
        }
        let mut out: Vec<Entity> = ids
            .into_iter()
            .filter_map(|id| self.entities.get(id as usize - 1).copied())
            .collect();
        out.sort_by_key(|e| e.to_bits());
        out.dedup();
        out
    }

    /// The selection outline: pixels outside the selected ids within `radius` px of a selected
    /// pixel (a crisp ring around the visible silhouette, through occluders' edges).
    #[must_use]
    pub fn outline_mask(&self, selected: &dyn Fn(Entity) -> bool, radius: i32) -> Vec<bool> {
        let (w, h) = (self.width as i32, self.height as i32);
        let inside: Vec<bool> = self
            .ids
            .iter()
            .map(|id| {
                (*id as usize)
                    .checked_sub(1)
                    .and_then(|i| self.entities.get(i))
                    .is_some_and(|e| selected(*e))
            })
            .collect();
        let mut mask = vec![false; inside.len()];
        if !inside.iter().any(|v| *v) {
            return mask;
        }
        for y in 0..h {
            for x in 0..w {
                let index = (y * w + x) as usize;
                if inside[index] {
                    continue;
                }
                'search: for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        let (nx, ny) = (x + dx, y + dy);
                        if nx >= 0 && ny >= 0 && nx < w && ny < h && inside[(ny * w + nx) as usize] {
                            mask[index] = true;
                            break 'search;
                        }
                    }
                }
            }
        }
        mask
    }
}

const SHADER: &str = r"
struct Globals { view_proj: mat4x4<f32>, eye: vec4<f32> };
struct Instance { model: mat4x4<f32>, id: vec4<u32> };
@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<storage, read> instances: array<Instance>;
struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) id: u32,
    @location(1) world: vec3<f32>,
};
@vertex fn vs_main(@location(0) position: vec3<f32>, @builtin(instance_index) instance: u32) -> VOut {
    let inst = instances[instance];
    let world = inst.model * vec4<f32>(position, 1.0);
    var out: VOut;
    out.clip = globals.view_proj * world;
    out.id = inst.id.x;
    out.world = world.xyz;
    return out;
}
struct FOut { @location(0) id: u32, @location(1) dist: f32 };
@fragment fn fs_main(in: VOut) -> FOut {
    var out: FOut;
    out.id = in.id;
    out.dist = distance(in.world, globals.eye.xyz);
    return out;
}
";

/// GPU geometry for one mesh: positions and indices.
struct GpuMesh {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
    /// Keeps the `Arc` alive so its address cannot be reused by another mesh while cached.
    _owner: Arc<CpuMesh>,
    last_used: u64,
}

/// The persistent id pass: pipeline + per-mesh buffers, scoped to one device.
struct IdPass {
    device_id: u64,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    meshes: HashMap<usize, GpuMesh>,
    frame: u64,
}

fn id_pass() -> &'static Mutex<Option<IdPass>> {
    static PASS: OnceLock<Mutex<Option<IdPass>>> = OnceLock::new();
    PASS.get_or_init(|| Mutex::new(None))
}

impl IdPass {
    fn new(device: &RenderDevice) -> Self {
        let gpu = &device.device;
        let module = gpu.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("editor id pass"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(SHADER)),
        });
        let layout = gpu.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor id pass"),
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
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = gpu.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("editor id pass"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = gpu.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("editor id pass"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
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
                entry_point: Some("fs_main"),
                targets: &[
                    Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::R32Uint,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    }),
                    Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::R32Float,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    }),
                ],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                // Double-sided: a click on a back face (thin shells, planes seen from below)
                // still selects the mesh.
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
            cache: None,
        });
        Self {
            device_id: device.device_id(),
            pipeline,
            layout,
            meshes: HashMap::new(),
            frame: 0,
        }
    }

    fn upload(&mut self, device: &RenderDevice, mesh: &Arc<CpuMesh>) {
        let key = Arc::as_ptr(mesh) as usize;
        let frame = self.frame;
        if let Some(cached) = self.meshes.get_mut(&key) {
            cached.last_used = frame;
            return;
        }
        let mut positions = Vec::with_capacity(mesh.vertices.len() * 12);
        for vertex in &mesh.vertices {
            for c in vertex.position {
                positions.extend_from_slice(&c.to_le_bytes());
            }
        }
        let mut indices = Vec::with_capacity(mesh.indices.len() * 4);
        for index in &mesh.indices {
            indices.extend_from_slice(&index.to_le_bytes());
        }
        let buffer = |label, bytes: &[u8], usage| {
            let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (bytes.len() as u64).max(4).next_multiple_of(4),
                usage: usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            device.queue.write_buffer(&buffer, 0, bytes);
            buffer
        };
        self.meshes.insert(
            key,
            GpuMesh {
                vertices: buffer("editor id vertices", &positions, wgpu::BufferUsages::VERTEX),
                indices: buffer("editor id indices", &indices, wgpu::BufferUsages::INDEX),
                index_count: mesh.indices.len() as u32,
                _owner: Arc::clone(mesh),
                last_used: frame,
            },
        );
    }
}

fn target(
    device: &RenderDevice,
    label: &str,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> wgpu::Texture {
    device.device.create_texture(&wgpu::TextureDescriptor {
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
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// Reads ids by pixel: an [`IdBuffer`] (the whole frame) or an [`IdWindow`] (a rectangle of
/// it). The picks in [`crate::viewport`] take any of them.
pub trait IdLookup {
    /// The entity at a pixel of the frame and its eye distance (`None` on background or
    /// outside what this holds).
    fn at(&self, x: u32, y: u32) -> Option<(Entity, f32)>;
}

impl IdLookup for IdBuffer {
    fn at(&self, x: u32, y: u32) -> Option<(Entity, f32)> {
        IdBuffer::at(self, x, y)
    }
}

/// A rectangle of an id buffer, in frame pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdRect {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl IdRect {
    /// The square of `radius` pixels around `center` (side `2 * radius + 1`), cut to a frame
    /// of `width` x `height`; `None` when `center` is outside the frame.
    #[must_use]
    pub fn around(center: (u32, u32), radius: u32, width: u32, height: u32) -> Option<Self> {
        if center.0 >= width || center.1 >= height {
            return None;
        }
        let x = center.0.saturating_sub(radius);
        let y = center.1.saturating_sub(radius);
        Some(Self {
            x,
            y,
            width: (center.0.saturating_add(radius).min(width - 1) - x) + 1,
            height: (center.1.saturating_add(radius).min(height - 1) - y) + 1,
        })
    }

    /// This rectangle cut to a frame of `width` x `height`; `None` when nothing is left.
    #[must_use]
    pub fn clamped(self, width: u32, height: u32) -> Option<Self> {
        if self.x >= width || self.y >= height || self.width == 0 || self.height == 0 {
            return None;
        }
        Some(Self {
            x: self.x,
            y: self.y,
            width: self.width.min(width - self.x),
            height: self.height.min(height - self.y),
        })
    }
}

/// A rectangle of the id buffer, read back alone (a click needs the pixel under the pointer,
/// not the frame). Pixels are addressed in frame coordinates; outside the rectangle there is
/// nothing ([`IdLookup::at`] gives `None`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IdWindow {
    /// Left edge of the window in the frame, pixels.
    pub x: u32,
    /// Top edge of the window in the frame, pixels.
    pub y: u32,
    /// Window width, pixels.
    pub width: u32,
    /// Window height, pixels.
    pub height: u32,
    /// Row-major ids of the window (0 = background; `n` = `entities[n - 1]`).
    pub ids: Vec<u32>,
    /// Row-major eye distance in metres (`f32::INFINITY` on background).
    pub distance: Vec<f32>,
    /// The entity table for this frame.
    pub entities: Vec<Entity>,
}

impl IdWindow {
    /// The entity at a frame pixel and its eye distance (`None` on background or outside the
    /// window).
    #[must_use]
    pub fn at(&self, x: u32, y: u32) -> Option<(Entity, f32)> {
        let (dx, dy) = (x.checked_sub(self.x)?, y.checked_sub(self.y)?);
        if dx >= self.width || dy >= self.height {
            return None;
        }
        let index = (dy * self.width + dx) as usize;
        let id = *self.ids.get(index)?;
        let entity = *self.entities.get((id as usize).checked_sub(1)?)?;
        Some((entity, self.distance.get(index).copied().unwrap_or(f32::INFINITY)))
    }
}

impl IdLookup for IdWindow {
    fn at(&self, x: u32, y: u32) -> Option<(Entity, f32)> {
        IdWindow::at(self, x, y)
    }
}

impl IdBuffer {
    /// The part of this buffer inside `rect` (cut to the buffer); empty when `rect` is outside.
    /// Reading the same rectangle on the GPU ([`render_id_window`]) gives the same window.
    #[must_use]
    pub fn window(&self, rect: IdRect) -> IdWindow {
        let Some(rect) = rect.clamped(self.width, self.height) else {
            return IdWindow {
                entities: self.entities.clone(),
                ..IdWindow::default()
            };
        };
        let mut ids = Vec::with_capacity((rect.width * rect.height) as usize);
        let mut distance = Vec::with_capacity(ids.capacity());
        for y in rect.y..rect.y + rect.height {
            let row = (y * self.width + rect.x) as usize;
            let end = row + rect.width as usize;
            ids.extend_from_slice(&self.ids[row..end]);
            distance.extend_from_slice(&self.distance[row..end]);
        }
        IdWindow {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            ids,
            distance,
            entities: self.entities.clone(),
        }
    }
}

/// Renders the id buffer for `draws` seen through `view_proj` from `eye`, and reads it back.
///
/// # Errors
///
/// GPU readback failed.
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_id_pass")]
pub fn render_id_buffer(
    device: &RenderDevice,
    draws: &[IdDraw],
    view_proj: Mat4,
    eye: [f32; 3],
    width: u32,
    height: u32,
) -> Result<IdBuffer, String> {
    let (width, height) = (width.max(1), height.max(1));
    let full = IdRect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let window = render_id_region(device, draws, view_proj, eye, (width, height), full)?;
    Ok(IdBuffer {
        width,
        height,
        ids: window.ids,
        distance: window.distance,
        entities: window.entities,
    })
}

/// [`render_id_buffer`] reading back only `rect` (cut to the frame): the id pass draws the
/// whole frame on the GPU, but two small copies replace two full-frame ones. The window holds
/// exactly the pixels [`render_id_buffer`] gives for that rectangle ([`IdBuffer::window`]).
///
/// # Errors
///
/// `rect` is outside the frame, or the GPU readback failed.
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_id_window")]
pub fn render_id_window(
    device: &RenderDevice,
    draws: &[IdDraw],
    view_proj: Mat4,
    eye: [f32; 3],
    width: u32,
    height: u32,
    rect: IdRect,
) -> Result<IdWindow, String> {
    let (width, height) = (width.max(1), height.max(1));
    let rect = rect
        .clamped(width, height)
        .ok_or_else(|| "id window outside the frame".to_string())?;
    render_id_region(device, draws, view_proj, eye, (width, height), rect)
}

/// The id pass over a `size` frame, reading back `rect` (already inside the frame).
fn render_id_region(
    device: &RenderDevice,
    draws: &[IdDraw],
    view_proj: Mat4,
    eye: [f32; 3],
    size: (u32, u32),
    rect: IdRect,
) -> Result<IdWindow, String> {
    let (width, height) = size;
    let mut guard = id_pass()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard
        .as_ref()
        .is_none_or(|pass| pass.device_id != device.device_id())
    {
        *guard = Some(IdPass::new(device));
    }
    let Some(pass) = guard.as_mut() else {
        return Err("id pass unavailable".into());
    };
    pass.frame += 1;
    for draw in draws {
        pass.upload(device, &draw.mesh);
    }
    // Evict meshes unused for a while (deleted or replaced assets).
    let frame = pass.frame;
    pass.meshes
        .retain(|_, mesh| frame.saturating_sub(mesh.last_used) < 600);

    let mut globals = Vec::with_capacity(80);
    for value in view_proj.to_cols_array() {
        globals.extend_from_slice(&value.to_le_bytes());
    }
    for value in [eye[0], eye[1], eye[2], 0.0] {
        globals.extend_from_slice(&value.to_le_bytes());
    }
    let mut instances = Vec::with_capacity(draws.len().max(1) * 80);
    for (index, draw) in draws.iter().enumerate() {
        for value in draw.model.to_cols_array() {
            instances.extend_from_slice(&value.to_le_bytes());
        }
        for value in [index as u32 + 1, 0, 0, 0] {
            instances.extend_from_slice(&value.to_le_bytes());
        }
    }
    if instances.is_empty() {
        instances.resize(80, 0);
    }
    let gpu = &device.device;
    let make = |label, bytes: &[u8], usage| {
        let buffer = gpu.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: bytes.len() as u64,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        device.queue.write_buffer(&buffer, 0, bytes);
        buffer
    };
    let globals = make("editor id globals", &globals, wgpu::BufferUsages::UNIFORM);
    let instances = make("editor id instances", &instances, wgpu::BufferUsages::STORAGE);
    let bind = gpu.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("editor id pass"),
        layout: &pass.layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: instances.as_entire_binding(),
            },
        ],
    });
    let id_tex = target(device, "editor ids", wgpu::TextureFormat::R32Uint, width, height);
    let dist_tex = target(
        device,
        "editor id distance",
        wgpu::TextureFormat::R32Float,
        width,
        height,
    );
    let depth_tex = target(
        device,
        "editor id depth",
        wgpu::TextureFormat::Depth32Float,
        width,
        height,
    );
    let id_view = id_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let dist_view = dist_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let depth_view = depth_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = gpu.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("editor id pass"),
    });
    {
        let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("editor id pass"),
            color_attachments: &[
                Some(wgpu::RenderPassColorAttachment {
                    view: &id_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                }),
                Some(wgpu::RenderPassColorAttachment {
                    view: &dist_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(f32::MAX),
                            g: 0.0,
                            b: 0.0,
                            a: 0.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                }),
            ],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
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
        rp.set_pipeline(&pass.pipeline);
        rp.set_bind_group(0, &bind, &[]);
        for (index, draw) in draws.iter().enumerate() {
            let Some(mesh) = pass.meshes.get(&(Arc::as_ptr(&draw.mesh) as usize)) else {
                continue;
            };
            if mesh.index_count == 0 {
                continue;
            }
            rp.set_vertex_buffer(0, mesh.vertices.slice(..));
            rp.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
            let instance = index as u32;
            rp.draw_indexed(0..mesh.index_count, 0, instance..instance + 1);
        }
    }
    let padded =
        (rect.width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = |label| {
        gpu.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: u64::from(padded) * u64::from(rect.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        })
    };
    let id_buf = readback("editor ids readback");
    let dist_buf = readback("editor id distance readback");
    for (texture, buffer) in [(&id_tex, &id_buf), (&dist_tex, &dist_buf)] {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: rect.x,
                    y: rect.y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(rect.height),
                },
            },
            wgpu::Extent3d {
                width: rect.width,
                height: rect.height,
                depth_or_array_layers: 1,
            },
        );
    }
    device.queue.submit([encoder.finish()]);
    let ids = read_words(gpu, &id_buf, rect.width, rect.height, padded)?;
    let distance = read_words(gpu, &dist_buf, rect.width, rect.height, padded)?
        .into_iter()
        .map(|bits| {
            let d = f32::from_bits(bits);
            if d >= f32::MAX * 0.5 { f32::INFINITY } else { d }
        })
        .collect();
    Ok(IdWindow {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
        ids,
        distance,
        entities: draws.iter().map(|draw| draw.entity).collect(),
    })
}

/// Maps a padded 32-bit-per-texel readback buffer into tightly packed words.
pub(crate) fn read_words(
    device: &wgpu::Device,
    buffer: &wgpu::Buffer,
    width: u32,
    height: u32,
    padded: u32,
) -> Result<Vec<u32>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|err| format!("GPU poll failed: {err}"))?;
    rx.recv()
        .map_err(|_| "map callback dropped".to_string())?
        .map_err(|err| format!("id readback failed: {err}"))?;
    let mapped = buffer
        .slice(..)
        .get_mapped_range()
        .map_err(|err| format!("id readback map failed: {err:?}"))?;
    let mut out = Vec::with_capacity((width * height) as usize);
    for y in 0..height as usize {
        let row = &mapped[y * padded as usize..y * padded as usize + width as usize * 4];
        out.extend(row.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b)));
    }
    drop(mapped);
    buffer.unmap();
    Ok(out)
}

/// The nearest [`regia_terrain::Terrain`] hit along the unit ray and its distance: the
/// runtime's hole-aware march ([`regia_pick::raycast_terrain`]), so a ray through a tunnel
/// mouth ([`regia_terrain::HolesMask`]) passes the cut and the mesh under it can be picked, and a
/// floating-origin rebase (`OriginOffset`) is honoured like in a game.
#[must_use]
pub fn pick_terrain(
    world: &World,
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
) -> Option<(Entity, f32)> {
    regia_pick::raycast_terrain(world, origin, direction, max_distance).map(|hit| (hit.entity, hit.distance))
}

// ---- Surface hits: point, normal and triangle (Tincal engine request F7) ----
//
// The id buffer names the entity under a pixel; a CPU ray cast against that entity's triangles
// refines it into the hit point, the surface normal and the triangle. The casts themselves live
// in the runtime crate `regia_pick` (the editor must not hold code a game needs); this module
// re-exports them and adds the editor's owner mapping ([`PickHit`]).

pub use regia_pick::{FaceCull, MeshHit, RayTriangle, ray_triangle, raycast_mesh};

/// A surface hit under a pixel or along a ray: the mesh entity, its triangle, the world point
/// and the normal ([`IdBuffer::hit`], [`raycast_entity`], [`raycast_world`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PickHit {
    /// The mesh entity whose triangle was hit (a model's part entity, not its owner).
    pub entity: Entity,
    /// What a click on it selects ([`crate::mesh_source::pick_owner`]).
    pub owner: Entity,
    /// Triangle index in the mesh's index list.
    pub triangle: u32,
    /// The triangle's three vertex indices.
    pub indices: [u32; 3],
    /// Weights of the triangle's vertices at the hit (sum 1).
    pub barycentric: Vec3,
    /// World-space hit point.
    pub position: Vec3,
    /// World-space unit normal of the triangle's front side ([`MeshHit::normal`]).
    pub normal: Vec3,
    /// Interpolated vertex normal, world space ([`MeshHit::smooth_normal`]).
    pub smooth_normal: Vec3,
    /// Distance from the ray origin, metres.
    pub distance: f32,
    /// The ray hit the triangle's front side.
    pub front_face: bool,
}

impl PickHit {
    fn new(world: &World, entity: Entity, hit: MeshHit) -> Self {
        Self {
            entity,
            owner: crate::mesh_source::pick_owner(world, entity),
            triangle: hit.triangle,
            indices: hit.indices,
            barycentric: hit.barycentric,
            position: hit.position,
            normal: hit.normal,
            smooth_normal: hit.smooth_normal,
            distance: hit.distance,
            front_face: hit.front_face,
        }
    }
}

/// Casts a ray against one mesh entity of `world` (its [`regia_render::MeshRenderer`],
/// `GlobalTransform` and, when present, [`regia_render::SkinJoints`]). `None` when it has no
/// mesh or the ray misses.
#[must_use]
pub fn raycast_entity(
    world: &World,
    entity: Entity,
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
) -> Option<PickHit> {
    let hit = regia_pick::raycast_mesh_entity(world, entity, origin, direction, cull)?;
    Some(PickHit::new(world, entity, hit))
}

/// The nearest hit over every mesh entity `filter` accepts (brute force with a bounding-box
/// early-out for rigid meshes).
#[must_use]
pub fn raycast_world(
    world: &World,
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
    filter: &dyn Fn(Entity) -> bool,
) -> Option<PickHit> {
    let _span = tracing::info_span!(target: "regia_editor", "raycast_world").entered();
    nearest(
        world
            .iter_entities()
            .filter(|e| e.contains::<regia_render::MeshRenderer>())
            .map(|e| e.id())
            .filter(|e| filter(*e)),
        world,
        origin,
        direction,
        cull,
    )
}

fn nearest(
    entities: impl Iterator<Item = Entity>,
    world: &World,
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
) -> Option<PickHit> {
    // One sweep of the pick caches for this query. The running distance is the cap: a hit at
    // exactly that distance is still cast, so the lower entity id still wins a tie.
    let _scope = regia_pick::pick_cache_scope();
    let mut best: Option<PickHit> = None;
    for entity in entities {
        let limit = best.as_ref().map_or(f32::INFINITY, |hit| hit.distance);
        let Some(hit) = raycast_entity_within(world, entity, origin, direction, cull, limit) else {
            continue;
        };
        let wins = best.as_ref().is_none_or(|current| {
            hit.distance < current.distance
                || (hit.distance == current.distance && hit.entity.to_bits() < current.entity.to_bits())
        });
        if wins {
            best = Some(hit);
        }
    }
    best
}

/// [`raycast_entity`] capped at `max_distance` (a hit at exactly that distance still counts).
fn raycast_entity_within(
    world: &World,
    entity: Entity,
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
    max_distance: f32,
) -> Option<PickHit> {
    let hit = regia_pick::raycast_mesh_entity_within(world, entity, origin, direction, cull, max_distance)?;
    Some(PickHit::new(world, entity, hit))
}

/// The mesh entities that draw for `owner`: itself and its parts.
pub(crate) fn mesh_entities_of(world: &World, owner: Entity) -> Vec<Entity> {
    let mut out = Vec::new();
    if world.get::<regia_render::MeshRenderer>(owner).is_some() {
        out.push(owner);
    }
    for e in world.iter_entities() {
        if e.get::<crate::mesh_source::MeshPart>()
            .is_some_and(|p| p.owner == owner)
        {
            out.push(e.id());
        }
    }
    out
}

/// Whether a mesh entity draws in the viewport (not hidden by the outliner eye, itself or its
/// owner).
pub(crate) fn draws_in_viewport(world: &World, entity: Entity) -> bool {
    let hidden = |e: Entity| {
        world.get::<regia_scene::layers::LayerParked>(e).is_some()
            || matches!(
                world.get::<regia_scene::prelude::Visibility>(e),
                Some(regia_scene::prelude::Visibility::Hidden)
            )
    };
    !hidden(entity) && !hidden(crate::mesh_source::pick_owner(world, entity))
}

/// The surface hit along a view ray narrowed by an id buffer pixel: the entity the id buffer
/// names (its mesh and part entities) plus every skinned mesh (drawn in bind pose by the id
/// pass), nearest first. When the named entity's triangles miss (an edge pixel), every mesh is
/// cast; with no id buffer too.
///
/// Two things the id buffer sees that a mesh ray cast does not: a proxy (an empty node's cube,
/// no triangles) in front of a mesh hides it, so a hit farther than the proxy is dropped; and
/// terrain (no id) in front hides it too, so a hit behind the terrain surface is dropped
/// ([`pick_in_frame`](crate::viewport::pick_in_frame) makes the same comparison). `eye` is the
/// camera the buffer was rendered with (its distances are from the eye, the ray starts on the
/// near plane).
pub(crate) fn pick_hit_along<I: IdLookup + ?Sized>(
    world: &World,
    ids: Option<&I>,
    pixel: (u32, u32),
    eye: Vec3,
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
) -> Option<PickHit> {
    let _span = tracing::info_span!(target: "regia_editor", "pick_hit").entered();
    let visible = |e: Entity| draws_in_viewport(world, e);
    let hit = pick_mesh_along(world, ids, pixel, eye, origin, direction, cull, &visible)?;
    // Terrain nearer than the mesh surface hides it (the id buffer has no terrain).
    if pick_terrain(world, origin, direction, hit.distance).is_some() {
        return None;
    }
    Some(hit)
}

#[allow(clippy::too_many_arguments)]
fn pick_mesh_along<I: IdLookup + ?Sized>(
    world: &World,
    ids: Option<&I>,
    pixel: (u32, u32),
    eye: Vec3,
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
    visible: &dyn Fn(Entity) -> bool,
) -> Option<PickHit> {
    let Some(ids) = ids else {
        return raycast_world(world, origin, direction, cull, visible);
    };
    let named = ids.at(pixel.0, pixel.1);
    let skinned = world
        .iter_entities()
        .filter(|e| e.contains::<regia_render::MeshRenderer>())
        .filter(|e| {
            e.get::<regia_render::SkinJoints>()
                .is_some_and(|s| !s.palette.is_empty())
        })
        .map(|e| e.id())
        .filter(|e| visible(*e));
    let mut candidates: Vec<Entity> = named.map(|(e, _)| mesh_entities_of(world, e)).unwrap_or_default();
    // A proxy names a pixel without any triangles: whatever it covers must be nearer than it.
    let proxy_limit = named
        .filter(|(e, _)| candidates.is_empty() && world.get::<regia_render::MeshRenderer>(*e).is_none())
        .map(|(_, d)| d - (origin - eye).length());
    candidates.extend(skinned);
    candidates.sort_by_key(|e| e.to_bits());
    candidates.dedup();
    let hit = nearest(candidates.into_iter(), world, origin, direction, cull);
    // Nothing among the named entity's triangles (or a proxy named the pixel): the nearest
    // mesh along the ray, which a proxy then has to be behind.
    let hit = match hit {
        None if named.is_some() => raycast_world(world, origin, direction, cull, visible),
        hit => hit,
    };
    match (hit, proxy_limit) {
        // Slack for the id pass's depth precision.
        (Some(hit), Some(limit)) if hit.distance > limit + (0.02 * limit.abs()).max(0.02) => None,
        (hit, _) => hit,
    }
}

impl IdBuffer {
    /// The surface under pixel `(x, y)` seen through `view` (the camera this buffer was
    /// rendered with): mesh entity, triangle, barycentric weights, world point and normal. The
    /// ray goes through the pixel's centre, where the id pass sampled it. Both faces count.
    #[must_use]
    pub fn hit(&self, world: &World, view: &crate::views::ViewCamera, x: u32, y: u32) -> Option<PickHit> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let size = regia_core::math::Vec2::new(self.width as f32, self.height as f32);
        let (origin, direction) = view.ray(regia_core::math::Vec2::new(x as f32 + 0.5, y as f32 + 0.5), size);
        pick_hit_along(
            world,
            Some(self),
            (x, y),
            view.eye,
            origin,
            direction,
            FaceCull::None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_pick::ray_meets_box;
    use regia_render::JointMatrix;

    fn buffer(w: u32, h: u32, ids: Vec<u32>, entities: Vec<Entity>) -> IdBuffer {
        IdBuffer {
            width: w,
            height: h,
            distance: vec![1.0; ids.len()],
            ids,
            entities,
        }
    }

    #[test]
    fn id_rect_around_is_cut_to_the_frame() {
        let rect = IdRect::around((5, 5), 2, 100, 50).expect("inside the frame");
        assert_eq!(
            rect,
            IdRect {
                x: 3,
                y: 3,
                width: 5,
                height: 5
            }
        );
        let corner = IdRect::around((0, 0), 3, 100, 50).expect("inside the frame");
        assert_eq!(
            corner,
            IdRect {
                x: 0,
                y: 0,
                width: 4,
                height: 4
            }
        );
        let far = IdRect::around((99, 49), 3, 100, 50).expect("inside the frame");
        assert_eq!(
            far,
            IdRect {
                x: 96,
                y: 46,
                width: 4,
                height: 4
            }
        );
        assert!(IdRect::around((100, 0), 3, 100, 50).is_none());
        assert!(
            IdRect {
                x: 100,
                y: 0,
                width: 2,
                height: 2
            }
            .clamped(100, 50)
            .is_none()
        );
        assert_eq!(
            IdRect {
                x: 98,
                y: 48,
                width: 9,
                height: 9
            }
            .clamped(100, 50),
            Some(IdRect {
                x: 98,
                y: 48,
                width: 2,
                height: 2
            })
        );
    }

    #[test]
    fn id_window_reads_the_same_pixels_as_the_buffer() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        let ids: Vec<u32> = (0..24).map(|i| [0, 1, 2][i % 3]).collect();
        let mut buf = buffer(6, 4, ids, vec![a, b]);
        for (i, d) in buf.distance.iter_mut().enumerate() {
            *d = i as f32;
        }
        let rect = IdRect {
            x: 1,
            y: 1,
            width: 3,
            height: 2,
        };
        let window = buf.window(rect);
        for y in 0..4 {
            for x in 0..6 {
                let inside = (1..4).contains(&x) && (1..3).contains(&y);
                let want = if inside { buf.at(x, y) } else { None };
                assert_eq!(window.at(x, y), want, "({x}, {y})");
            }
        }
        // Through the trait too.
        assert_eq!(IdLookup::at(&window, 2, 2), IdLookup::at(&buf, 2, 2));
        // Entirely outside: empty.
        assert_eq!(
            buf.window(IdRect {
                x: 9,
                y: 9,
                width: 2,
                height: 2
            })
            .at(9, 9),
            None
        );
    }

    /// A unit box mesh entity centred at `at`.
    fn box_at(world: &mut World, at: Vec3) -> Entity {
        let (vertices, indices) = regia_render::box_mesh([0.5; 3]);
        let mesh = std::sync::Arc::new(regia_render::CpuMesh { vertices, indices });
        let material = std::sync::Arc::new(regia_render::LabMaterial::default());
        world
            .spawn((
                regia_render::MeshRenderer::new(mesh, material),
                regia_scene::GlobalTransform(regia_core::glam::Affine3A::from_translation(at)),
            ))
            .id()
    }

    /// An id buffer of one pixel naming `entity` at `distance` from the eye.
    fn one_pixel(entity: Entity, distance: f32) -> IdBuffer {
        IdBuffer {
            width: 1,
            height: 1,
            ids: vec![1],
            distance: vec![distance],
            entities: vec![entity],
        }
    }

    #[test]
    fn a_proxy_in_front_hides_the_mesh_behind_it() {
        let mut world = World::new();
        let rock = box_at(&mut world, Vec3::new(0.0, 0.0, -5.0));
        let proxy = world.spawn_empty().id();
        let eye = Vec3::ZERO;
        let hit =
            |ids: &IdBuffer| pick_hit_along(&world, Some(ids), (0, 0), eye, eye, Vec3::NEG_Z, FaceCull::None);
        // The proxy cube covers the pixel 2 m from the eye: the rock at 4.5 m is hidden.
        assert!(hit(&one_pixel(proxy, 2.0)).is_none());
        // The proxy behind the rock: the rock is hit.
        let behind = hit(&one_pixel(proxy, 20.0)).expect("rock in front of the proxy");
        assert_eq!(behind.entity, rock);
        // The mesh itself named: the usual hit.
        assert_eq!(hit(&one_pixel(rock, 4.5)).map(|h| h.entity), Some(rock));
    }

    fn flat_terrain(world: &mut World) {
        use regia_terrain::{CdlodConfig, MinMaxPyramid, Terrain, TerrainLayers, TerrainMap};
        let n = 129;
        let map = TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 256.0, 1.0, 0.0).expect("map");
        let minmax = MinMaxPyramid::build(&map, 4).expect("pyramid");
        world.spawn((
            regia_scene::prelude::Transform::default(),
            Terrain {
                map,
                cdlod: CdlodConfig::default(),
                root_level: 4,
                minmax,
                layers: TerrainLayers::default(),
                holes: None,
                water: None,
            },
        ));
    }

    /// The editor's terrain march is the runtime's: a floating-origin rebase moves the heightfield
    /// under a local ray (it sits 1024 m away in local space) and the cast still finds it.
    #[test]
    fn terrain_pick_honours_the_origin_offset() {
        let mut world = World::new();
        flat_terrain(&mut world);
        let local = Vec3::new(-1024.0, 10.0, -1024.0);
        assert!(pick_terrain(&world, local, Vec3::NEG_Y, 100.0).is_none());
        world.insert_resource(regia_scene::streaming::OriginOffset([1024.0, 0.0, 1024.0]));
        let (_, distance) = pick_terrain(&world, local, Vec3::NEG_Y, 100.0).expect("hit after the rebase");
        assert!((distance - 10.0).abs() < 1e-3, "{distance}");
    }

    #[test]
    fn terrain_in_front_hides_the_mesh_behind_it() {
        let mut world = World::new();
        flat_terrain(&mut world);
        let buried = box_at(&mut world, Vec3::new(0.0, -3.0, 0.0));
        let standing = box_at(&mut world, Vec3::new(20.0, 0.5, 0.0));
        let eye = Vec3::new(0.0, 10.0, 0.0);
        // Straight down onto the ground: the rock under it is hidden, with or without an id
        // buffer (the named entity is the one the buffer saw, here the rock behind the hill).
        let down = |ids: Option<&IdBuffer>| {
            pick_hit_along(&world, ids, (0, 0), eye, eye, Vec3::NEG_Y, FaceCull::None)
        };
        assert!(down(None).is_none());
        assert!(down(Some(&one_pixel(buried, 13.0))).is_none());
        // A rock standing on the terrain is hit as before.
        let origin = Vec3::new(20.0, 10.0, 0.0);
        let hit = pick_hit_along(
            &world,
            None::<&IdBuffer>,
            (0, 0),
            origin,
            origin,
            Vec3::NEG_Y,
            FaceCull::None,
        )
        .expect("rock above the ground");
        assert_eq!(hit.entity, standing);
    }

    #[test]
    fn pixel_and_rect_lookups_map_ids_to_entities() {
        let a = Entity::from_bits(7 | (1 << 32));
        let b = Entity::from_bits(9 | (1 << 32));
        let ids = vec![
            0, 1, 1, 0, //
            0, 1, 2, 2, //
            0, 0, 2, 2,
        ];
        let buf = buffer(4, 3, ids, vec![a, b]);
        assert_eq!(buf.at(1, 0).map(|h| h.0), Some(a));
        assert_eq!(buf.at(3, 2).map(|h| h.0), Some(b));
        assert!(buf.at(0, 0).is_none());
        assert!(buf.at(9, 9).is_none());
        assert_eq!(buf.entities_in_rect((0, 0), (1, 2)), vec![a]);
        let both = buf.entities_in_rect((3, 2), (0, 0));
        assert_eq!(both.len(), 2);
    }

    fn close(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-4
    }

    #[test]
    fn ray_hits_a_known_triangle_with_its_barycentric_weights() {
        let (a, b, c) = (Vec3::ZERO, Vec3::X, Vec3::Y);
        let hit =
            ray_triangle(Vec3::new(0.25, 0.25, 1.0), Vec3::NEG_Z, a, b, c, FaceCull::None).expect("hit");
        assert!((hit.t - 1.0).abs() < 1e-6);
        assert!(close(hit.barycentric, Vec3::new(0.5, 0.25, 0.25)));
        assert!(hit.front_face, "counter-clockwise seen from +Z faces the ray");
        // A direction that is not unit scales `t`, not the point.
        let long = ray_triangle(
            Vec3::new(0.25, 0.25, 1.0),
            Vec3::NEG_Z * 2.0,
            a,
            b,
            c,
            FaceCull::None,
        )
        .expect("hit");
        assert!((long.t - 0.5).abs() < 1e-6);
        // Corners and edges are inside: a ray along a shared edge cannot fall through.
        assert!(ray_triangle(Vec3::new(0.5, 0.5, 1.0), Vec3::NEG_Z, a, b, c, FaceCull::None).is_some());
        assert!(ray_triangle(Vec3::new(0.0, 0.0, 1.0), Vec3::NEG_Z, a, b, c, FaceCull::None).is_some());
    }

    #[test]
    fn back_faces_hit_unless_culled_and_misses_return_none() {
        let (a, b, c) = (Vec3::ZERO, Vec3::X, Vec3::Y);
        let from_below = Vec3::new(0.25, 0.25, -1.0);
        let back = ray_triangle(from_below, Vec3::Z, a, b, c, FaceCull::None).expect("back face");
        assert!(!back.front_face);
        assert!(ray_triangle(from_below, Vec3::Z, a, b, c, FaceCull::Back).is_none());
        // Outside the triangle, parallel to it, behind the origin, degenerate.
        assert!(ray_triangle(Vec3::new(0.8, 0.8, 1.0), Vec3::NEG_Z, a, b, c, FaceCull::None).is_none());
        assert!(ray_triangle(Vec3::new(0.25, 0.25, 1.0), Vec3::X, a, b, c, FaceCull::None).is_none());
        assert!(ray_triangle(Vec3::new(0.25, 0.25, 1.0), Vec3::Z, a, b, c, FaceCull::None).is_none());
        assert!(ray_triangle(Vec3::new(0.25, 0.0, 1.0), Vec3::NEG_Z, a, b, b, FaceCull::None).is_none());
    }

    fn cube() -> CpuMesh {
        let (vertices, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
        CpuMesh { vertices, indices }
    }

    #[test]
    fn mesh_cast_applies_the_model_and_reports_the_nearest_triangle() {
        let mesh = cube();
        // Scaled ×2 on X, turned 90° about Y, moved to (3, 1, 0): the unit cube's +X face
        // (scaled to x = ±1 locally) now faces −Z at z = −1.
        let model = Mat4::from_scale_rotation_translation(
            Vec3::new(2.0, 1.0, 1.0),
            regia_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            Vec3::new(3.0, 1.0, 0.0),
        );
        let origin = Vec3::new(3.1, 1.2, -10.0);
        let hit =
            raycast_mesh(&mesh, &mesh.indices, model, &[], origin, Vec3::Z, FaceCull::None).expect("hit");
        assert!(
            close(hit.position, Vec3::new(3.1, 1.2, -1.0)),
            "{:?}",
            hit.position
        );
        assert!((hit.distance - 9.0).abs() < 1e-4);
        assert!(
            close(hit.normal, Vec3::NEG_Z),
            "the face toward the ray: {:?}",
            hit.normal
        );
        assert!(close(hit.smooth_normal, Vec3::NEG_Z));
        assert!(hit.front_face);
        let tri = hit.triangle as usize;
        assert_eq!(
            hit.indices,
            [
                mesh.indices[3 * tri],
                mesh.indices[3 * tri + 1],
                mesh.indices[3 * tri + 2]
            ]
        );
        // The barycentric weights rebuild the hit point from the triangle's corners.
        let corner = |i: u32| model.transform_point3(Vec3::from_array(mesh.vertices[i as usize].position));
        let rebuilt = hit.barycentric.x * corner(hit.indices[0])
            + hit.barycentric.y * corner(hit.indices[1])
            + hit.barycentric.z * corner(hit.indices[2]);
        assert!(close(rebuilt, hit.position));
        // From inside the cube only back faces are in reach.
        let inside = raycast_mesh(
            &mesh,
            &mesh.indices,
            model,
            &[],
            Vec3::new(3.0, 1.0, 0.0),
            Vec3::Z,
            FaceCull::None,
        )
        .expect("inside hit");
        assert!(!inside.front_face);
        assert!(close(inside.position, Vec3::new(3.0, 1.0, 1.0)));
        assert!(
            raycast_mesh(
                &mesh,
                &mesh.indices,
                model,
                &[],
                Vec3::new(3.0, 1.0, 0.0),
                Vec3::Z,
                FaceCull::Back
            )
            .is_none()
        );
        // A ray that passes beside it.
        assert!(
            raycast_mesh(
                &mesh,
                &mesh.indices,
                model,
                &[],
                Vec3::new(9.0, 1.0, -10.0),
                Vec3::Z,
                FaceCull::None
            )
            .is_none()
        );
    }

    #[test]
    fn a_mirroring_model_reports_faces_as_the_renderer_winds_them() {
        // The renderer does not flip the winding of a mirroring model (importers flip the
        // indices instead), so the cube's outside faces wind clockwise on screen: back faces.
        let mesh = cube();
        let model = Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0));
        let origin = Vec3::new(0.1, 0.1, 5.0);
        let near = raycast_mesh(
            &mesh,
            &mesh.indices,
            model,
            &[],
            origin,
            Vec3::NEG_Z,
            FaceCull::None,
        )
        .expect("near face");
        assert!(!near.front_face);
        assert!(close(near.position, Vec3::new(0.1, 0.1, 0.5)));
        assert!(
            close(near.normal, Vec3::NEG_Z),
            "the front side's normal: {:?}",
            near.normal
        );
        let far = raycast_mesh(
            &mesh,
            &mesh.indices,
            model,
            &[],
            origin,
            Vec3::NEG_Z,
            FaceCull::Back,
        )
        .expect("far face");
        assert!(far.front_face);
        assert!(close(far.position, Vec3::new(0.1, 0.1, -0.5)));
    }

    #[test]
    fn skinned_meshes_are_cast_where_the_palette_puts_them() {
        let mut mesh = cube();
        for v in &mut mesh.vertices {
            v.joints = [0, 1, 0, 0];
            v.weights = [0.5, 0.5, 0.0, 0.0];
        }
        // Half on a joint at +4 X and half on one at +2 X: the cube is drawn around x = 3, and
        // the entity's model matrix lifts it by 1 (world = model × skinned, as the shader does).
        let palette = [
            JointMatrix::translation(4.0, 0.0, 0.0),
            JointMatrix::translation(2.0, 0.0, 0.0),
        ];
        let model = Mat4::from_translation(Vec3::Y);
        let hit = raycast_mesh(
            &mesh,
            &mesh.indices,
            model,
            &palette,
            Vec3::new(3.0, 1.0, 5.0),
            Vec3::NEG_Z,
            FaceCull::None,
        )
        .expect("skinned hit");
        assert!(
            close(hit.position, Vec3::new(3.0, 1.0, 0.5)),
            "{:?}",
            hit.position
        );
        assert!(close(hit.normal, Vec3::Z));
        // Where the bind pose would be, nothing.
        assert!(
            raycast_mesh(
                &mesh,
                &mesh.indices,
                model,
                &palette,
                Vec3::new(0.0, 1.0, 5.0),
                Vec3::NEG_Z,
                FaceCull::None
            )
            .is_none()
        );
        // A rotating joint turns the normal with it.
        let turned = [JointMatrix::from_mat4(Mat4::from_rotation_y(std::f32::consts::FRAC_PI_2)); 2];
        let hit = raycast_mesh(
            &mesh,
            &mesh.indices,
            Mat4::IDENTITY,
            &turned,
            Vec3::new(0.1, 0.1, 5.0),
            Vec3::NEG_Z,
            FaceCull::None,
        )
        .expect("turned hit");
        assert!(close(hit.normal, Vec3::Z) && close(hit.smooth_normal, Vec3::Z));
    }

    #[test]
    fn out_of_range_indices_and_bad_input_are_skipped() {
        let mesh = cube();
        let indices = [0, 1, 999];
        assert!(
            raycast_mesh(
                &mesh,
                &indices,
                Mat4::IDENTITY,
                &[],
                Vec3::new(0.0, 0.0, 5.0),
                Vec3::NEG_Z,
                FaceCull::None
            )
            .is_none()
        );
        assert!(
            raycast_mesh(
                &mesh,
                &mesh.indices,
                Mat4::ZERO,
                &[],
                Vec3::new(0.0, 0.0, 5.0),
                Vec3::NEG_Z,
                FaceCull::None
            )
            .is_none()
        );
        assert!(
            raycast_mesh(
                &mesh,
                &mesh.indices,
                Mat4::IDENTITY,
                &[],
                Vec3::new(0.0, 0.0, 5.0),
                Vec3::ZERO,
                FaceCull::None
            )
            .is_none()
        );
        assert!(ray_meets_box(
            Vec3::new(0.0, 0.0, 5.0),
            Vec3::NEG_Z,
            (Vec3::splat(-0.5), Vec3::splat(0.5))
        ));
        assert!(!ray_meets_box(
            Vec3::new(0.0, 0.0, 5.0),
            Vec3::Z,
            (Vec3::splat(-0.5), Vec3::splat(0.5))
        ));
        assert!(!ray_meets_box(
            Vec3::new(2.0, 0.0, 5.0),
            Vec3::NEG_Z,
            (Vec3::splat(-0.5), Vec3::splat(0.5))
        ));
    }

    #[test]
    fn outline_rings_the_selected_silhouette_only() {
        let a = Entity::from_bits(3 | (1 << 32));
        // A 2×2 block of `a` in a 6×6 frame.
        let mut ids = vec![0u32; 36];
        for (x, y) in [(2, 2), (3, 2), (2, 3), (3, 3)] {
            ids[y * 6 + x] = 1;
        }
        let buf = buffer(6, 6, ids, vec![a]);
        let mask = buf.outline_mask(&|e| e == a, 1);
        let count = mask.iter().filter(|m| **m).count();
        // The ring around a 2×2 block at radius 1 is the 4×4 square minus the block.
        assert_eq!(count, 16 - 4);
        assert!(!mask[2 * 6 + 2], "the selection itself is not outlined");
        let none = buf.outline_mask(&|_| false, 1);
        assert!(none.iter().all(|m| !m));
    }
    #[test]
    fn coincident_meshes_resolve_by_entity_id_not_iteration_order() {
        let mut world = World::new();
        let a = box_at(&mut world, Vec3::ZERO);
        let b = box_at(&mut world, Vec3::ZERO);
        let (lo, hi) = if a.to_bits() < b.to_bits() { (a, b) } else { (b, a) };
        let origin = Vec3::new(0.0, 0.0, 5.0);
        for order in [[lo, hi], [hi, lo]] {
            let hit = nearest(
                order.into_iter(),
                &world,
                origin,
                Vec3::NEG_Z,
                FaceCull::default(),
            )
            .expect("hit");
            assert_eq!(hit.entity, lo);
        }
    }

    #[test]
    fn id_draws_sort_by_entity_and_keep_part_order() {
        let mut world = World::new();
        let a = box_at(&mut world, Vec3::ZERO);
        let b = box_at(&mut world, Vec3::X);
        let (vertices, indices) = regia_render::box_mesh([0.5; 3]);
        let mesh = std::sync::Arc::new(regia_render::CpuMesh { vertices, indices });
        let draw = |entity, x: f32| IdDraw {
            mesh: Arc::clone(&mesh),
            model: Mat4::from_translation(Vec3::new(x, 0.0, 0.0)),
            entity,
        };
        let mut draws = vec![draw(b, 1.0), draw(a, 2.0), draw(b, 3.0), draw(a, 4.0)];
        sort_id_draws(&mut draws);
        let order: Vec<(Entity, f32)> = draws.iter().map(|d| (d.entity, d.model.w_axis.x)).collect();
        // `Entity`'s own order, which is not spawn order in bevy_ecs 0.20 (ADR-1389).
        let expected = if a < b {
            [(a, 2.0), (a, 4.0), (b, 1.0), (b, 3.0)]
        } else {
            [(b, 1.0), (b, 3.0), (a, 2.0), (a, 4.0)]
        };
        assert_eq!(order, expected);
    }
}
