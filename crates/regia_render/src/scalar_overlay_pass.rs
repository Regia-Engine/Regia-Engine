// SPDX-License-Identifier: MPL-2.0
//! The scalar overlay pass (`crate::scalar_overlay`): meshes coloured by a ramp, drawn over the
//! final image.
//!
//! [`crate::FrameRenderer`] calls [`ScalarOverlayPass::prepare`] for each submitted mesh that
//! has an overlay while it walks the frame's meshes, then [`ScalarOverlayPass::record`] after the
//! post stack and translucent layers, next to the gizmo pass. The pass keeps its own copy of
//! every overlaid mesh's vertices and indices (by mesh key), a values buffer, a joint palette
//! and a uniform per overlaid entity (by object key). Everything uploads on change only: a steady
//! frame, or the same overlay in a new component, writes nothing and creates nothing. Entries
//! idle for [`IDLE_FRAMES`] frames are dropped.

use crate::frame_work::counted::*;
use std::collections::HashMap;
use std::sync::Arc;

use regia_core::math::Mat4;

use crate::device::{TrackedDevice, TrackedQueue};
use crate::scalar_overlay::{OverlayFit, RAMP_LUT_SIZE, Ramp, ScalarOverlay, ScalarOverlayStats, sanitize};
use crate::{FrameMesh, JointMatrix, LabVertex, RenderError};

const SHADER: &str = include_str!("../shaders/scalar_overlay.wgsl");

/// Frames an entry may go unused before its buffers are dropped.
const IDLE_FRAMES: u64 = 240;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewUniform {
    clip_from_world: [[f32; 4]; 4],
    viewport: [f32; 4],
    flags: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct ObjectUniform {
    model: [[f32; 4]; 4],
    /// Range start, range end, opacity.
    params: [f32; 4],
    lut: [[f32; 4]; RAMP_LUT_SIZE],
}

/// Where and how the pass draws (the gizmo pass's target).
pub(crate) struct OverlayTarget<'a> {
    /// The final colour attachment (LDR, after tonemap).
    pub view: &'a wgpu::TextureView,
    /// Its format.
    pub format: wgpu::TextureFormat,
    /// Viewport rectangle in target pixels (x, y, w, h).
    pub viewport: [f32; 4],
    /// The frame's scene depth (any size; sampled by UV inside the viewport).
    pub depth: &'a wgpu::TextureView,
    /// `depth` is multisampled.
    pub multisampled_depth: bool,
    /// Unjittered view-projection of the frame (standard depth 0..1).
    pub clip_from_world: Mat4,
}

struct Geometry {
    vb: wgpu::Buffer,
    ib: wgpu::Buffer,
    vertex_count: usize,
    index_count: u32,
    last_used: u64,
}

struct ObjectGpu {
    uniform: wgpu::Buffer,
    uploaded: Option<ObjectUniform>,
    /// The ramp `uploaded.lut` was baked from.
    ramp: Option<Ramp>,
    joints: wgpu::Buffer,
    joint_capacity: usize,
    joint_bytes: Vec<u8>,
    group: wgpu::BindGroup,
    values: wgpu::Buffer,
    values_len: usize,
    /// The values last uploaded. Held, so a new `Arc` never reuses its address.
    values_src: Arc<[f32]>,
    last_used: u64,
}

struct Job {
    object_key: u64,
    mesh_key: u64,
}

struct Layouts {
    view: [wgpu::BindGroupLayout; 2],
    object: wgpu::BindGroupLayout,
    pipeline: [wgpu::PipelineLayout; 2],
}

/// GPU state of the overlay pass (pipelines are built on first use per variant).
#[derive(Default)]
pub(crate) struct ScalarOverlayPass {
    layouts: Option<Layouts>,
    modules: HashMap<bool, wgpu::ShaderModule>,
    pipelines: HashMap<(wgpu::TextureFormat, bool), wgpu::RenderPipeline>,
    view_buffer: Option<wgpu::Buffer>,
    view_group: Option<(wgpu::TextureView, bool, wgpu::BindGroup)>,
    geometry: HashMap<u64, Geometry>,
    objects: HashMap<u64, ObjectGpu>,
    jobs: Vec<Job>,
    /// Entities whose mismatch was already logged.
    warned: std::collections::HashSet<u64>,
    frame: u64,
    stats: ScalarOverlayStats,
}

fn buffer(device: &TrackedDevice, label: &'static str, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(4),
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn entry(binding: u32, visibility: wgpu::ShaderStages, ty: wgpu::BindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty,
        count: None,
    }
}

impl Layouts {
    fn new(device: &wgpu::Device) -> Self {
        let view = [false, true].map(|multisampled| {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("scalar overlay view"),
                entries: &[
                    entry(
                        0,
                        wgpu::ShaderStages::VERTEX_FRAGMENT,
                        wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: wgpu::BufferSize::new(size_of::<ViewUniform>() as u64),
                        },
                    ),
                    entry(
                        1,
                        wgpu::ShaderStages::FRAGMENT,
                        wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Depth,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled,
                        },
                    ),
                ],
            })
        });
        let object = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scalar overlay object"),
            entries: &[
                entry(
                    0,
                    wgpu::ShaderStages::VERTEX_FRAGMENT,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(size_of::<ObjectUniform>() as u64),
                    },
                ),
                entry(
                    1,
                    wgpu::ShaderStages::VERTEX,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(size_of::<JointMatrix>() as u64),
                    },
                ),
            ],
        });
        let pipeline = [0, 1].map(|i| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("scalar overlay"),
                bind_group_layouts: &[Some(&view[i]), Some(&object)],
                immediate_size: 0,
            })
        });
        Self {
            view,
            object,
            pipeline,
        }
    }
}

/// The shader's defs for one module variant.
pub(crate) fn shader_defs(multisampled_depth: bool) -> HashMap<String, naga_oil::compose::ShaderDefValue> {
    let mut defs = HashMap::new();
    if multisampled_depth {
        defs.insert(
            "DEPTH_MULTISAMPLED".to_string(),
            naga_oil::compose::ShaderDefValue::Bool(true),
        );
    }
    defs
}

/// The overlay shader source (for the shader validation gate).
pub(crate) const fn shader_source() -> &'static str {
    SHADER
}

/// The joint matrices as bytes, at least one (identity) so the storage binding is never empty.
///
/// Every call allocates. A steady pose compares in place (`joints_match`) and does not call this.
fn joint_bytes(joints: &[JointMatrix]) -> Vec<u8> {
    #[cfg(test)]
    joint_alloc_probe::note_copy();
    if joints.is_empty() {
        bytemuck::bytes_of(&JointMatrix::identity()).to_vec()
    } else {
        bytemuck::cast_slice(joints).to_vec()
    }
}

/// Palette length stored for `joints`: one identity matrix when the mesh is rigid.
fn joint_palette_count(joints: &[JointMatrix]) -> usize {
    joints.len().max(1)
}

/// True when `stored` is already the palette `joints` would upload.
///
/// An empty palette matches the single identity matrix. The compare borrows `joints`; it does
/// not build a `Vec`.
fn joints_match(stored: &[u8], joints: &[JointMatrix]) -> bool {
    if joints.is_empty() {
        let identity = JointMatrix::identity();
        stored == bytemuck::bytes_of(&identity)
    } else {
        stored == bytemuck::cast_slice::<JointMatrix, u8>(joints)
    }
}

/// Counts of `joint_bytes` copies and joint uploads on this thread.
///
/// Thread-local so a parallel test that also calls `joint_bytes` does not move another test's delta.
#[cfg(test)]
mod joint_alloc_probe {
    use std::cell::Cell;

    thread_local! {
        static COPIES: Cell<u64> = const { Cell::new(0) };
        static UPLOADS: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn note_copy() {
        COPIES.with(|count| count.set(count.get() + 1));
    }

    pub(super) fn note_upload() {
        UPLOADS.with(|count| count.set(count.get() + 1));
    }

    pub(super) fn copies() -> u64 {
        COPIES.with(Cell::get)
    }

    pub(super) fn uploads() -> u64 {
        UPLOADS.with(Cell::get)
    }
}

/// The values the GPU reads: NaN and infinities made finite.
fn gpu_values(values: &[f32]) -> Vec<f32> {
    values.iter().map(|&v| sanitize(v)).collect()
}

impl ScalarOverlayPass {
    /// Starts a frame: forgets the previous frame's draws and drops long-idle entries.
    pub(crate) fn begin_frame(&mut self) {
        self.frame += 1;
        self.jobs.clear();
        self.stats.drawn = 0;
        self.stats.mismatched = 0;
        if self.warned.len() > 1024 {
            self.warned.clear();
        }
        let frame = self.frame;
        self.geometry.retain(|_, g| frame - g.last_used <= IDLE_FRAMES);
        self.objects.retain(|_, o| frame - o.last_used <= IDLE_FRAMES);
    }

    /// What the pass did so far ([`crate::FrameRenderer::scalar_overlay_stats`]).
    pub(crate) fn stats(&self) -> ScalarOverlayStats {
        self.stats
    }

    /// Uploads whatever changed for `mesh`'s overlay and queues its draw. A values length that
    /// does not match the vertex count queues nothing.
    pub(crate) fn prepare(
        &mut self,
        device: &TrackedDevice,
        queue: &TrackedQueue,
        mesh: &FrameMesh<'_>,
        overlay: &ScalarOverlay,
    ) {
        let opacity = overlay.effective_opacity();
        if mesh.vertices.is_empty() || mesh.indices.len() < 3 {
            return;
        }
        match overlay.fit(mesh.vertices.len()) {
            OverlayFit::Exact => {
                self.warned.remove(&mesh.object_key);
            }
            fit => {
                self.stats.mismatched += 1;
                // Once per entity until it matches again, not every frame.
                if self.warned.insert(mesh.object_key) {
                    tracing::warn!(
                        target: "regia_render",
                        ?fit,
                        object = mesh.object_key,
                        "scalar overlay values do not match the mesh's vertices; not drawn"
                    );
                }
                return;
            }
        }
        if opacity <= 0.0 {
            return;
        }
        let layouts = self.layouts.get_or_insert_with(|| Layouts::new(device));
        let frame = self.frame;

        // Geometry: a copy of the mesh, by key, re-copied when its size changes.
        let index_count = mesh.indices.len() as u32;
        let stale = self
            .geometry
            .get(&mesh.mesh_key)
            .is_none_or(|g| g.vertex_count != mesh.vertices.len() || g.index_count != index_count);
        if stale {
            let vb = buffer(
                device,
                "scalar overlay vertices",
                std::mem::size_of_val(mesh.vertices) as u64,
                wgpu::BufferUsages::VERTEX,
            );
            let ib = buffer(
                device,
                "scalar overlay indices",
                std::mem::size_of_val(mesh.indices) as u64,
                wgpu::BufferUsages::INDEX,
            );
            queue.write_buffer(&vb, 0, bytemuck::cast_slice::<LabVertex, u8>(mesh.vertices));
            queue.write_buffer(&ib, 0, bytemuck::cast_slice(mesh.indices));
            self.stats.geometry_uploads += 1;
            self.geometry.insert(
                mesh.mesh_key,
                Geometry {
                    vb,
                    ib,
                    vertex_count: mesh.vertices.len(),
                    index_count,
                    last_used: frame,
                },
            );
        }
        if let Some(geometry) = self.geometry.get_mut(&mesh.mesh_key) {
            geometry.last_used = frame;
        }

        // Per-entity state. The palette length sizes the buffer; the bytes are built only when
        // the stored pose differs, so a steady frame does not allocate them.
        let joint_count = joint_palette_count(mesh.joints);
        let object = self.objects.entry(mesh.object_key).or_insert_with(|| {
            let uniform = buffer(
                device,
                "scalar overlay uniform",
                size_of::<ObjectUniform>() as u64,
                wgpu::BufferUsages::UNIFORM,
            );
            let joint_buffer = buffer(
                device,
                "scalar overlay joints",
                (joint_count * size_of::<JointMatrix>()) as u64,
                wgpu::BufferUsages::STORAGE,
            );
            let group = object_group(device, &layouts.object, &uniform, &joint_buffer);
            let values = buffer(
                device,
                "scalar overlay values",
                (mesh.vertices.len() * 4) as u64,
                wgpu::BufferUsages::VERTEX,
            );
            ObjectGpu {
                uniform,
                uploaded: None,
                ramp: None,
                joints: joint_buffer,
                joint_capacity: joint_count,
                joint_bytes: Vec::new(),
                group,
                values,
                values_len: mesh.vertices.len(),
                values_src: Arc::from(Vec::new()),
                last_used: frame,
            }
        });
        object.last_used = frame;

        // Joints: a bigger palette replaces the buffer (and the bind group); the same palette
        // writes nothing.
        if joint_count > object.joint_capacity {
            object.joints = buffer(
                device,
                "scalar overlay joints",
                (joint_count * size_of::<JointMatrix>()) as u64,
                wgpu::BufferUsages::STORAGE,
            );
            object.joint_capacity = joint_count;
            object.group = object_group(device, &layouts.object, &object.uniform, &object.joints);
            object.joint_bytes.clear();
        }
        if !joints_match(&object.joint_bytes, mesh.joints) {
            let joints = joint_bytes(mesh.joints);
            queue.write_buffer(&object.joints, 0, &joints);
            #[cfg(test)]
            joint_alloc_probe::note_upload();
            object.joint_bytes = joints;
        }

        // Values: one write per distinct `Arc`.
        if object.values_len != mesh.vertices.len() {
            object.values = buffer(
                device,
                "scalar overlay values",
                (mesh.vertices.len() * 4) as u64,
                wgpu::BufferUsages::VERTEX,
            );
            object.values_len = mesh.vertices.len();
            object.values_src = Arc::from(Vec::new());
        }
        if !Arc::ptr_eq(&object.values_src, &overlay.values) {
            queue.write_buffer(
                &object.values,
                0,
                bytemuck::cast_slice(&gpu_values(&overlay.values)),
            );
            object.values_src = overlay.values.clone();
            self.stats.value_uploads += 1;
        }

        // Uniform: model, range, opacity and the ramp table, written when any of them changed.
        let lut = match &object.ramp {
            Some(ramp) if *ramp == overlay.ramp => object.uploaded.map(|u| u.lut),
            _ => None,
        }
        .unwrap_or_else(|| overlay.ramp.lut());
        let uniform = ObjectUniform {
            model: mesh.model,
            params: [overlay.range.0, overlay.range.1, opacity, 0.0],
            lut,
        };
        if object.uploaded != Some(uniform) {
            queue.write_buffer(&object.uniform, 0, bytemuck::bytes_of(&uniform));
            object.uploaded = Some(uniform);
        }
        if object.ramp.as_ref() != Some(&overlay.ramp) {
            object.ramp = Some(overlay.ramp.clone());
        }
        self.stats.drawn += 1;
        self.jobs.push(Job {
            object_key: mesh.object_key,
            mesh_key: mesh.mesh_key,
        });
    }

    fn module(
        &mut self,
        device: &wgpu::Device,
        multisampled: bool,
    ) -> Result<wgpu::ShaderModule, RenderError> {
        if let Some(module) = self.modules.get(&multisampled) {
            return Ok(module.clone());
        }
        let naga_module =
            crate::shader_lib::compose_source(SHADER, "scalar_overlay.wgsl", shader_defs(multisampled))?;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scalar_overlay.wgsl"),
            source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(naga_module)),
        });
        self.modules.insert(multisampled, module.clone());
        Ok(module)
    }

    fn pipeline(
        &mut self,
        device: &TrackedDevice,
        format: wgpu::TextureFormat,
        multisampled: bool,
    ) -> Result<wgpu::RenderPipeline, RenderError> {
        if let Some(pipeline) = self.pipelines.get(&(format, multisampled)) {
            return Ok(pipeline.clone());
        }
        let module = self.module(device, multisampled)?;
        let layouts = self.layouts.get_or_insert_with(|| Layouts::new(device));
        let values = [wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32,
            offset: 0,
            shader_location: 7,
        }];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("scalar overlay"),
            layout: Some(&layouts.pipeline[usize::from(multisampled)]),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[
                    Some(LabVertex::layout()),
                    Some(wgpu::VertexBufferLayout {
                        array_stride: 4,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &values,
                    }),
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(device).as_deref(),
        });
        self.pipelines.insert((format, multisampled), pipeline.clone());
        Ok(pipeline)
    }

    /// Records the queued draws into `encoder`. Does nothing without any.
    #[tracing::instrument(skip_all)]
    pub(crate) fn record(
        &mut self,
        device: &TrackedDevice,
        queue: &TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        target: &OverlayTarget<'_>,
    ) -> Result<(), RenderError> {
        if self.jobs.is_empty() {
            return Ok(());
        }
        let pipeline = self.pipeline(device, target.format, target.multisampled_depth)?;
        let Some(layouts) = self.layouts.as_ref() else {
            return Ok(());
        };
        let view_uniform = ViewUniform {
            clip_from_world: target.clip_from_world.to_cols_array_2d(),
            viewport: target.viewport,
            flags: [u32::from(target.format.is_srgb()), 0, 0, 0],
        };
        let view_buffer = self.view_buffer.get_or_insert_with(|| {
            buffer(
                device,
                "scalar overlay view",
                size_of::<ViewUniform>() as u64,
                wgpu::BufferUsages::UNIFORM,
            )
        });
        queue.write_buffer(view_buffer, 0, bytemuck::bytes_of(&view_uniform));
        // The view group binds the frame's depth view: kept while the same view comes back.
        let reuse = matches!(
            &self.view_group,
            Some((depth, multisampled, _))
                if depth == target.depth && *multisampled == target.multisampled_depth
        );
        if !reuse {
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("scalar overlay view"),
                layout: &layouts.view[usize::from(target.multisampled_depth)],
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: view_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(target.depth),
                    },
                ],
            });
            self.view_group = Some((target.depth.clone(), target.multisampled_depth, group));
        }
        let Some((_, _, view_group)) = self.view_group.as_ref() else {
            return Ok(());
        };

        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("scalar overlay"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let [x, y, w, h] = target.viewport;
        pass.set_viewport(x, y, w.max(1.0), h.max(1.0), 0.0, 1.0);
        pass.counted_set_pipeline(&pipeline);
        pass.counted_set_bind_group(0, view_group, &[]);
        for job in &self.jobs {
            let (Some(geometry), Some(object)) = (
                self.geometry.get(&job.mesh_key),
                self.objects.get(&job.object_key),
            ) else {
                continue;
            };
            pass.counted_set_bind_group(1, &object.group, &[]);
            pass.set_vertex_buffer(0, geometry.vb.slice(..));
            pass.set_vertex_buffer(1, object.values.slice(..));
            pass.set_index_buffer(geometry.ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.counted_draw_indexed(0..geometry.index_count, 0, 0..1);
        }
        Ok(())
    }
}

fn object_group(
    device: &TrackedDevice,
    layout: &wgpu::BindGroupLayout,
    uniform: &wgpu::Buffer,
    joints: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("scalar overlay object"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: joints.as_entire_binding(),
            },
        ],
    })
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "scalar_overlay.wgsl",
            "OverlayView",
            ViewUniform {
                clip_from_world,
                viewport,
                flags
            }
        ),
        crate::contract_row!(
            "scalar_overlay.wgsl",
            "OverlayObject",
            ObjectUniform { model, params, lut }
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_layouts_fit_the_shader() {
        // OverlayView: mat4 + vec4 + vec4<u32>; OverlayObject: mat4 + vec4 + 64 vec4.
        assert_eq!(size_of::<ViewUniform>(), 96);
        assert_eq!(size_of::<ObjectUniform>(), 64 + 16 + 16 * RAMP_LUT_SIZE);
        assert_eq!(RAMP_LUT_SIZE, 64, "the shader's `lut` array and index clamp");
    }

    #[test]
    fn every_variant_composes() {
        for multisampled in [false, true] {
            crate::shader_lib::compose_source(SHADER, "scalar_overlay.wgsl", shader_defs(multisampled))
                .unwrap_or_else(|err| panic!("scalar_overlay.wgsl ({multisampled}): {err}"));
        }
    }

    #[test]
    fn gpu_values_are_finite() {
        let values = gpu_values(&[1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY]);
        assert_eq!(values, [1.0, 0.0, 1.0e30, -1.0e30]);
        assert!(joint_bytes(&[]).len() == size_of::<JointMatrix>());
    }

    #[test]
    fn joints_match_does_not_copy_a_palette() {
        let identity = joint_bytes(&[]);
        let mut palette = vec![JointMatrix::identity(); 256];
        palette[3] = JointMatrix::translation(1.0, 2.0, 3.0);
        let stored = joint_bytes(&palette);
        let before = joint_alloc_probe::copies();
        assert!(joints_match(&identity, &[]));
        assert!(
            !joints_match(&[], &[]),
            "an empty store is not the identity palette"
        );
        assert!(joints_match(&stored, &palette));
        assert!(!joints_match(&identity, &palette));
        let mut moved = palette.clone();
        moved[3] = JointMatrix::translation(4.0, 0.0, 0.0);
        assert!(!joints_match(&stored, &moved));
        assert_eq!(
            joint_alloc_probe::copies(),
            before,
            "comparing a pose allocated a palette"
        );
    }

    fn triangle() -> (Vec<crate::LabVertex>, Vec<u32>) {
        let normal = [0.0, 0.0, 1.0];
        (
            vec![
                crate::LabVertex::from_pos_normal_uv([0.0, 0.0, 0.0], normal, [0.0, 0.0]),
                crate::LabVertex::from_pos_normal_uv([1.0, 0.0, 0.0], normal, [1.0, 0.0]),
                crate::LabVertex::from_pos_normal_uv([0.0, 1.0, 0.0], normal, [0.0, 1.0]),
            ],
            vec![0, 1, 2],
        )
    }

    fn overlay_mesh<'a>(
        vertices: &'a [crate::LabVertex],
        indices: &'a [u32],
        material: &'a crate::LabMaterial,
        joints: &'a [JointMatrix],
        object_key: u64,
    ) -> crate::FrameMesh<'a> {
        crate::FrameMesh {
            is_static: false,
            mesh_key: 0x0A11_0305,
            material_key: 1,
            object_key,
            vertices,
            indices,
            material,
            model: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            joints,
            receiver_layer: 0,
            shadow: crate::ShadowCast::Off,
            uv1: &[],
            lod_dither: 0.0,
        }
    }

    /// A second prepare of an unchanged pose copies no joint bytes and uploads none.
    ///
    /// The pre-fix `prepare` called `joint_bytes` before the compare, so this steady assert
    /// failed with a copy delta of 1 (48 bytes rigid, 12,288 bytes at 256 joints).
    ///
    /// Skips unless `VK_ICD_FILENAMES` names lavapipe, so a desktop GPU is never opened.
    #[test]
    fn audit_g3_steady_overlay_pose_does_not_copy_joints() {
        let icd = std::env::var("VK_ICD_FILENAMES").unwrap_or_default();
        if !icd.contains("lvp") {
            return;
        }
        let device = crate::RenderDevice::headless().unwrap_or_else(|err| panic!("lavapipe device: {err}"));
        assert_eq!(
            device.adapter.get_info().device_type,
            wgpu::DeviceType::Cpu,
            "VK_ICD_FILENAMES selected a non-CPU adapter"
        );
        let (vertices, indices) = triangle();
        let material = crate::LabMaterial::default();
        let overlay = ScalarOverlay::new(vec![0.0_f32, 0.5, 1.0]);
        let tracked = device.tracked();
        let queue = device.tracked_queue();
        let mut pass = ScalarOverlayPass::default();

        let copies = joint_alloc_probe::copies();
        let uploads = joint_alloc_probe::uploads();
        let rigid = overlay_mesh(&vertices, &indices, &material, &[], 1);
        pass.prepare(&tracked, &queue, &rigid, &overlay);
        assert_eq!(
            joint_alloc_probe::copies(),
            copies + 1,
            "the first rigid pose copies once"
        );
        assert_eq!(
            joint_alloc_probe::uploads(),
            uploads + 1,
            "the first rigid pose uploads once"
        );
        pass.prepare(&tracked, &queue, &rigid, &overlay);
        assert_eq!(
            joint_alloc_probe::copies(),
            copies + 1,
            "a steady rigid pose copied the joint palette"
        );
        assert_eq!(
            joint_alloc_probe::uploads(),
            uploads + 1,
            "a steady rigid pose uploaded joints"
        );

        let mut palette = vec![JointMatrix::identity(); 256];
        palette[3] = JointMatrix::translation(1.0, 0.0, 0.0);
        let copies = joint_alloc_probe::copies();
        let uploads = joint_alloc_probe::uploads();
        {
            let skinned = overlay_mesh(&vertices, &indices, &material, &palette, 2);
            pass.prepare(&tracked, &queue, &skinned, &overlay);
            assert_eq!(
                joint_alloc_probe::copies(),
                copies + 1,
                "the first 256-joint pose copies once"
            );
            assert_eq!(
                joint_alloc_probe::uploads(),
                uploads + 1,
                "the first 256-joint pose uploads once"
            );
            pass.prepare(&tracked, &queue, &skinned, &overlay);
            assert_eq!(
                joint_alloc_probe::copies(),
                copies + 1,
                "a steady 256-joint pose copied the palette"
            );
            assert_eq!(
                joint_alloc_probe::uploads(),
                uploads + 1,
                "a steady 256-joint pose uploaded"
            );
        }
        palette[3] = JointMatrix::translation(2.0, 0.0, 0.0);
        let moved = overlay_mesh(&vertices, &indices, &material, &palette, 2);
        pass.prepare(&tracked, &queue, &moved, &overlay);
        assert_eq!(
            joint_alloc_probe::copies(),
            copies + 2,
            "a changed pose copies once"
        );
        assert_eq!(
            joint_alloc_probe::uploads(),
            uploads + 2,
            "a changed pose uploads once"
        );
        pass.prepare(&tracked, &queue, &moved, &overlay);
        assert_eq!(
            joint_alloc_probe::copies(),
            copies + 2,
            "the changed pose copied again on the next prepare"
        );
        assert_eq!(
            joint_alloc_probe::uploads(),
            uploads + 2,
            "the changed pose uploaded again"
        );
    }
}
