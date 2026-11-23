// SPDX-License-Identifier: MPL-2.0
//! Temporal upscaler (`specs/26` § 4).
//!
//! The colour pass renders at `render_scale` with a Halton(2, 3) sub-pixel jitter.
//! This pass reprojects that image into the output resolution with the motion of the
//! closest surface around each pixel, a cubic history fetch (Keys' cubic at a = −1) and a YCoCg
//! variance clip (Karis, SIGGRAPH 2014; Salvi and Pedersen, GDC 2016; Yang, Liu &
//! Salvi, JCGT 2020 § 4.3; `shaders/taau.wgsl`). The first frame has no history and
//! is a bilinear upsample.
//! A plain upsample (no temporal blend) is the same pass with history disabled,
//! used when `render_scale` is below 1 without TAA.

use crate::frame_work::counted::*;
use std::collections::HashMap;

use crate::RenderError;
use crate::shader_lib::compose_source;

/// Strength of the motion-adaptive sharpen after the resolve (`shaders/taau_sharpen.wgsl`)
/// at full motion; 0 turns the pass off.
pub const MOTION_SHARPEN: f32 = 0.5;
/// Motion (output pixels per frame) at which the sharpen reaches full strength.
pub const MOTION_SHARPEN_FULL_PX: f32 = 1.0;

/// Highest history weight a resolve accepts. Motion uses 0.9; a held camera's running
/// mean climbs toward this while it converges (`frame.rs`, `HELD_CONVERGE_FRAMES`).
pub const MAX_HISTORY_WEIGHT: f32 = 0.98;

/// Halton sequence base `base`, 1-indexed sample `index` (index 0 returns 0).
#[must_use]
pub fn halton(index: u32, base: u32) -> f32 {
    let mut f = 1.0f32;
    let mut r = 0.0f32;
    let mut i = index;
    let base = base.max(2);
    while i > 0 {
        f /= base as f32;
        r += f * (i % base) as f32;
        i /= base;
    }
    r
}

/// Sub-pixel jitter in NDC for a render target of `width`×`height`.
///
/// The range is ±½ pixel: `(halton − 0.5) · 2 / size`.
#[must_use]
pub fn halton_jitter(frame: u64, width: u32, height: u32) -> [f32; 2] {
    let n = (frame as u32).max(1);
    let jx = halton(n, 2) - 0.5;
    let jy = halton(n, 3) - 0.5;
    [jx * 2.0 / width.max(1) as f32, jy * 2.0 / height.max(1) as f32]
}

/// Applies `jitter` (NDC) to a DirectX right-handed perspective matrix.
///
/// `clip.w = −view_z` and that factor lives in column 2, so subtracting the
/// jitter from `z_axis.xy` adds it in NDC.
#[must_use]
pub fn jitter_projection(mut proj: regia_core::glam::Mat4, jitter: [f32; 2]) -> regia_core::glam::Mat4 {
    proj.z_axis.x -= jitter[0];
    proj.z_axis.y -= jitter[1];
    proj
}

/// Smallest velocity joint buffer: 64 KiB, about 680 joints of current and previous palettes,
/// so a scene's first characters do not grow it on the frame.
/// Floor of the velocity joint buffer: 128 KiB holds nine 80-joint or three 256-joint moving
/// instances (96 B per joint, current and previous palette), so ordinary DO27 crowds never
/// grow it mid-play.
const VELOCITY_JOINTS_MIN_BYTES: u64 = 128 * 1024;

/// One object drawn into the motion-vector target.
#[derive(Clone, Copy, Debug)]
pub struct VelocityInstance {
    /// Current model matrix.
    pub model: [[f32; 4]; 4],
    /// Previous frame's model matrix (current, if this object is new).
    pub prev_model: [[f32; 4]; 4],
    /// A skinned object: the first joint of this frame's palette and of last frame's in the
    /// `joints` of [`TaauPass::record_velocity`]. `None` for a rigid object.
    pub skin: Option<[u32; 2]>,
    /// Position in the frame's draw order. Where objects overlap, the later one keeps
    /// its motion, as if drawn one by one in this order (a depth test on the key), so
    /// instances of one mesh draw together.
    pub order: u32,
}

/// The velocity depth value of draw-order position `order`: `(order + 1) / 2²⁴`, exact
/// in `f32` and in a `Depth32Float` target, so `GreaterEqual` keeps the latest draw.
#[must_use]
pub(crate) fn order_depth(order: u32) -> f32 {
    (order.min((1 << 24) - 2) + 1) as f32 / (1u32 << 24) as f32
}

/// Geometry for one instanced velocity draw. Buffers are reference-counted clones.
pub struct VelocityDraw {
    /// Vertex buffer.
    pub vb: wgpu::Buffer,
    /// Index buffer.
    pub ib: wgpu::Buffer,
    /// Index count.
    pub index_count: u32,
    /// First index in `ib` (a mesh-arena range).
    pub first_index: u32,
    /// First vertex in `vb`.
    pub base_vertex: i32,
    /// First slot in the object storage buffer (`instance_index`).
    pub slot: u32,
    /// Consecutive slots drawn from `slot`.
    pub instances: u32,
    /// Draw both faces. A double-sided material (or a frame that already disables culling)
    /// uses the cull-none velocity pipeline, the same choice as its colour and depth draws.
    /// Part of the batch key: two instances merge only when this matches.
    pub cull_none: bool,
    /// Skinned instances (`VelocityInstance::skin`): drawn with the skinning velocity
    /// pipeline. Part of the batch key.
    pub skinned: bool,
    /// A material graph with a vertex stage: its velocity pipeline and material group (the
    /// motion of the displaced surface). That pipeline already carries the graph's cull mode.
    pub graph: Option<VelocityGraph>,
    /// A compute-skinned twin (`crate::skin_compute`): `vb` holds this frame's skinned
    /// vertices and this buffer last frame's (vertex buffer slot 1), drawn with the
    /// pre-skinned velocity pipeline. Part of the batch key (a twin never batches).
    pub prev_vb: Option<wgpu::Buffer>,
}

/// The material-graph part of a [`VelocityDraw`].
pub struct VelocityGraph {
    /// Velocity pipeline built against [`TaauPass::velocity_layouts`] plus the material group.
    pub pipeline: wgpu::RenderPipeline,
    /// The material bind group (group 2).
    pub material: wgpu::BindGroup,
    /// Second UV stream (vertex buffer slot 1) when the graph reads it.
    pub uv1: Option<wgpu::Buffer>,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VelocityFrame {
    view_proj: [[f32; 4]; 4],
    unjittered: [[f32; 4]; 4],
    prev_view_proj: [[f32; 4]; 4],
    inv_view_proj: [[f32; 4]; 4],
    /// x = 1 when the occlusion depth (group 0, binding 1) is this frame's scene depth.
    occlusion: [f32; 4],
    /// Time, previous time, wind x, wind z. Read when [`Self::clock_live`] `.x` is above 0.5.
    clock: [f32; 4],
    /// `x` is 1 when a graph reads [`Self::clock`] instead of its material slot 0.
    clock_live: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct VelocityObject {
    model: [[f32; 4]; 4],
    prev_model: [[f32; 4]; 4],
    skin: [u32; 4],
    pad: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct TaauParams {
    inv_size: [f32; 4],
    blend: [f32; 4],
    /// x = history scale (pre-exposure change since the history was written; 0 reads as 1).
    extra: [f32; 4],
}

struct Tex {
    /// Owned with the view so the history texture drops with the pass.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

/// History, motion vectors and the upscale pipeline.
#[derive(Default)]
pub struct TaauPass {
    /// Scale applied to the history on the next [`Self::resolve`] (0 reads as 1).
    history_scale: f32,
    /// Graph clock copied into the velocity uniform ([`Self::set_graph_clock`]).
    graph_clock: [f32; 4],
    /// The velocity shader reads [`Self::graph_clock`] instead of material slot 0.
    graph_clock_live: bool,
    /// The next resolve's reactive mask ([`Self::set_reactive`]).
    reactive: Option<wgpu::TextureView>,
    pipeline: Option<wgpu::RenderPipeline>,
    bgl: Option<wgpu::BindGroupLayout>,
    sampler: Option<wgpu::Sampler>,
    params: Option<wgpu::Buffer>,
    history: [Option<Tex>; 2],
    which: usize,
    /// `history[1 - which]` holds a resolve of this output size (the last frame's image).
    last_output: Option<(u32, u32)>,
    /// The sharpened display image (output size); the history stays unsharpened.
    display: Option<Tex>,
    sharpen_pipe: Option<wgpu::RenderPipeline>,
    sharpen_bgl: Option<wgpu::BindGroupLayout>,
    sharpen_params: Option<wgpu::Buffer>,
    /// Sharpen group, keyed by the resolved and velocity views.
    sharpen_cache: crate::bind_cache::BindCache<(wgpu::TextureView, wgpu::TextureView)>,
    velocity: Option<Tex>,
    /// Last frame's velocity target (swapped with `velocity` each frame): the resolve
    /// compares the motion the history pixel had with the motion it is fetched for.
    prev_velocity: Option<Tex>,
    /// `prev_velocity` holds a frame of this size.
    prev_velocity_valid: bool,
    /// Device-wide pipeline registry ([`Self::share_pipelines`]); `None` builds per pass.
    shared: Option<std::sync::Arc<crate::device_pipelines::DevicePipelines>>,
    velocity_pipe: Option<wgpu::RenderPipeline>,
    /// Cull-none velocity pipeline (double-sided materials, and a frame that draws both faces).
    /// Built with [`Self::velocity_pipe`] so the first use, or [`Self::prewarm`], creates both.
    velocity_pipe_ds: Option<wgpu::RenderPipeline>,
    /// Skinned-mesh velocity pipelines (back-face, cull-none): `vs_skinned` skins each vertex
    /// with this frame's and last frame's palettes. Built with [`Self::velocity_pipe`], so the
    /// warm-up builds them and a character met later compiles none.
    velocity_skin_pipe: Option<wgpu::RenderPipeline>,
    velocity_skin_pipe_ds: Option<wgpu::RenderPipeline>,
    /// Compute-skinned velocity pipelines (back-face, cull-none): `vs_preskinned` reads this
    /// frame's and last frame's skinned vertices. Built with the others.
    velocity_preskin_pipe: Option<wgpu::RenderPipeline>,
    velocity_preskin_pipe_ds: Option<wgpu::RenderPipeline>,
    /// Joint palettes of the skinned velocity instances (object group, binding 1).
    velocity_joints: Option<wgpu::Buffer>,
    velocity_joint_cap: u64,
    /// Bytes the joint buffer holds (changed-range uploads).
    velocity_joint_bytes: Vec<u8>,
    /// Fullscreen camera-motion pass from depth (`RenderProfile::depth_velocity`).
    camera_velocity_pipe: Option<wgpu::RenderPipeline>,
    camera_depth_bgl: Option<wgpu::BindGroupLayout>,
    camera_depth_bg: Option<(wgpu::TextureView, wgpu::BindGroup)>,
    velocity_frame_bgl: Option<wgpu::BindGroupLayout>,
    velocity_obj_bgl: Option<wgpu::BindGroupLayout>,
    velocity_frame: Option<wgpu::Buffer>,
    velocity_objects: Option<wgpu::Buffer>,
    velocity_obj_cap: u64,
    /// Objects the velocity buffer is sized for at least ([`Self::reserve`]).
    reserved: u32,
    /// Bytes the velocity object buffer holds (changed-range uploads).
    velocity_obj_bytes: Vec<u8>,
    /// Object pack reused across frames. Grown when the instance count exceeds its capacity.
    velocity_pack: Vec<VelocityObject>,
    /// Times [`Self::record_velocity`] allocated a fresh object pack. Test builds only.
    #[cfg(test)]
    velocity_pack_allocs: u32,
    /// Bytes the last object upload wrote (0 when the matrices already matched). Test builds only.
    #[cfg(test)]
    velocity_last_written: usize,
    /// Draw-order keys of the velocity pass (`Depth32Float`, [`order_depth`]).
    velocity_order: Option<(wgpu::TextureView, u32, u32)>,
    dummy: Option<Tex>,
    /// 1×1 depth bound when the frame has no single-sample scene depth.
    dummy_depth: Option<wgpu::TextureView>,
    /// Velocity frame group, keyed by the occlusion depth view bound with the uniform.
    velocity_frame_bg: Option<(wgpu::TextureView, wgpu::BindGroup)>,
    /// Velocity object group, keyed by the object and joint buffers (rebuilt when one grows).
    velocity_obj_cache: crate::bind_cache::BindCache<(wgpu::Buffer, wgpu::Buffer)>,
    /// Resolve group, keyed by the five views and the params buffer.
    resolve_cache: crate::bind_cache::BindCache<(
        wgpu::TextureView,
        wgpu::TextureView,
        wgpu::TextureView,
        wgpu::TextureView,
        wgpu::TextureView,
        wgpu::TextureView,
        wgpu::Buffer,
    )>,
    /// Skip vertex and index binds the velocity pass already holds (ADR-508).
    bind_trims: bool,
    /// Binds [`Self::record_velocity`] recorded and skipped since the frame last took them.
    velocity_binds: crate::submission::BindCounts,
}

impl TaauPass {
    /// Whether the next velocity passes skip the vertex and index binds they already hold
    /// ([`crate::FrameRenderer::set_submission_trims`]).
    pub(crate) fn set_bind_trims(&mut self, trims: bool) {
        self.bind_trims = trims;
    }

    /// Binds the velocity passes recorded and skipped since the last call (zero when none
    /// ran); the frame takes them once per frame.
    pub(crate) fn take_velocity_binds(&mut self) -> crate::submission::BindCounts {
        std::mem::take(&mut self.velocity_binds)
    }

    /// Sizes the velocity object buffer for at least `instances` objects (no mid-game
    /// reallocation and bind-group rebuild when a view reaches that many).
    pub fn reserve(&mut self, instances: u32) {
        self.reserved = self.reserved.max(instances);
    }

    /// Builds the velocity pipelines once per device in `registry` (a texture view's pass or a
    /// stand-in renderer then compiles nothing). Idempotent; call before the first frame.
    pub(crate) fn share_pipelines(
        &mut self,
        registry: &std::sync::Arc<crate::device_pipelines::DevicePipelines>,
    ) {
        if self.shared.is_none() {
            self.shared = Some(registry.clone());
        }
    }

    /// Builds the back-face and cull-none velocity pipelines (and the camera-motion pass).
    ///
    /// [`crate::FrameRenderer::prewarm`] calls this so a later frame that meets a double-sided
    /// material creates no velocity pipeline. A 1×1 target is replaced by the first real frame.
    ///
    /// # Errors
    ///
    /// The velocity shader failed to compose.
    pub(crate) fn prewarm(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        self.ensure_velocity(device, 1, 1)
    }

    /// Frame warms (`FrameRenderer::prewarm_mid_play`): builds the resolve and sharpen
    /// pipelines when this pass has drawn no frame yet (the warm frame used TSR), so a switch
    /// back to TAAU mid-play, or a scaled frame without TAA, compiles nothing. Its 1×1 targets
    /// are replaced by the first real frame. A pass that has resolved is left as it is.
    ///
    /// # Errors
    ///
    /// The shaders failed to compose.
    pub(crate) fn prewarm_resolve(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        if self.pipeline.is_none() {
            self.ensure_resolve(device, 1, 1)?;
        }
        if self.sharpen_pipe.is_none() {
            self.ensure_sharpen(device, 1, 1)?;
        }
        Ok(())
    }

    /// This frame's velocity texture, when the pass has run (`previous uv − current uv` in `xy`).
    pub(crate) fn velocity_texture(&self) -> Option<(&wgpu::Texture, u32, u32)> {
        self.velocity
            .as_ref()
            .map(|tex| (&tex.texture, tex.width, tex.height))
    }

    /// The clock the next [`Self::record_velocity`] writes. `live` is set when a material clock
    /// or scene wind is active, so time-reading graphs share that one uniform.
    pub(crate) fn set_graph_clock(&mut self, clock: [f32; 4], live: bool) {
        self.graph_clock = clock;
        self.graph_clock_live = live;
    }

    /// Uploads motion-vector state and records the velocity pass.
    ///
    /// `joints` holds the palettes the skinned `instances` index (`VelocityInstance::skin`).
    #[allow(clippy::too_many_arguments)]
    pub fn record_velocity(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        width: u32,
        height: u32,
        jittered: [[f32; 4]; 4],
        unjittered: [[f32; 4]; 4],
        prev: [[f32; 4]; 4],
        instances: &[VelocityInstance],
        joints: &[crate::JointMatrix],
        draws: &[VelocityDraw],
        depth: Option<&wgpu::TextureView>,
        occlusion: Option<&wgpu::TextureView>,
    ) -> Result<(), RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        // Last frame's motion becomes the previous target; this frame draws into the other.
        std::mem::swap(&mut self.velocity, &mut self.prev_velocity);
        self.ensure_velocity(device, width.max(1), height.max(1))?;
        let frame = VelocityFrame {
            view_proj: jittered,
            unjittered,
            prev_view_proj: prev,
            inv_view_proj: regia_core::glam::Mat4::from_cols_array_2d(&jittered)
                .inverse()
                .to_cols_array_2d(),
            occlusion: [if occlusion.is_some() { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0],
            clock: self.graph_clock,
            clock_live: [f32::from(self.graph_clock_live), 0.0, 0.0, 0.0],
        };
        belt.write(
            device,
            encoder,
            self.velocity_frame.as_ref().expect("velocity frame"),
            0,
            bytemuck::bytes_of(&frame),
        );
        let count = instances.len();
        #[cfg(test)]
        let pack_before = self.velocity_pack.capacity();
        self.velocity_pack.clear();
        self.velocity_pack.extend(instances.iter().map(|instance| {
            VelocityObject {
                model: instance.model,
                prev_model: instance.prev_model,
                skin: instance
                    .skin
                    .map_or([0; 4], |[current, prev]| [current, prev, 0, 0]),
                pad: [order_depth(instance.order).to_bits(), 0, 0, 0],
            }
        }));
        #[cfg(test)]
        if pack_before < count {
            self.velocity_pack_allocs = self.velocity_pack_allocs.saturating_add(1);
        }
        let needed = ((count * std::mem::size_of::<VelocityObject>()) as u64)
            .max(std::mem::size_of::<VelocityObject>() as u64)
            .max(u64::from(self.reserved) * std::mem::size_of::<VelocityObject>() as u64);
        if self.velocity_obj_cap < needed {
            let cap = needed.next_power_of_two();
            self.velocity_objects = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("velocity objects"),
                size: cap,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.velocity_obj_cap = cap;
            self.velocity_obj_cache.clear();
            self.velocity_obj_bytes.clear();
        }
        // Only the objects whose matrices changed (static objects keep their bytes).
        // Take the pack so the upload can borrow the byte cache at the same time, then put it back.
        let pack = std::mem::take(&mut self.velocity_pack);
        let buffer = self.velocity_objects.clone();
        let written = if let Some(buffer) = buffer.as_ref() {
            let bytes = bytemuck::cast_slice(&pack);
            crate::staging::write_changed(belt, device, encoder, buffer, bytes, &mut self.velocity_obj_bytes)
        } else {
            0
        };
        self.velocity_pack = pack;
        #[cfg(test)]
        {
            self.velocity_last_written = written;
        }
        #[cfg(not(test))]
        let _ = written;
        let joint_bytes: &[u8] = bytemuck::cast_slice(joints);
        let joint_needed = (joint_bytes.len() as u64).max(VELOCITY_JOINTS_MIN_BYTES);
        if self.velocity_joint_cap < joint_needed {
            let cap = joint_needed.next_power_of_two();
            self.velocity_joints = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("velocity joints"),
                size: cap,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.velocity_joint_cap = cap;
            self.velocity_obj_cache.clear();
            self.velocity_joint_bytes.clear();
        }
        if !joint_bytes.is_empty()
            && let Some(buffer) = self.velocity_joints.as_ref()
        {
            crate::staging::write_changed(
                belt,
                device,
                encoder,
                buffer,
                joint_bytes,
                &mut self.velocity_joint_bytes,
            );
        }
        let occlusion_view = occlusion
            .cloned()
            .or_else(|| self.dummy_depth.clone())
            .expect("velocity occlusion depth");
        if self
            .velocity_frame_bg
            .as_ref()
            .is_none_or(|(v, _)| *v != occlusion_view)
        {
            let layout = self.velocity_frame_bgl.clone().expect("velocity frame bgl");
            let frame_buf = self.velocity_frame.clone().expect("velocity frame");
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("velocity frame"),
                layout: &layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: frame_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&occlusion_view),
                    },
                ],
            });
            self.velocity_frame_bg = Some((occlusion_view, bg));
        }
        let frame_bg = self
            .velocity_frame_bg
            .as_ref()
            .map(|(_, bg)| bg.clone())
            .expect("velocity frame bg");
        let objects = self.velocity_objects.clone().expect("velocity objects");
        let joint_buf = self.velocity_joints.clone().expect("velocity joints");
        let obj_layout = self.velocity_obj_bgl.clone().expect("velocity obj bgl");
        let obj_bg = self
            .velocity_obj_cache
            .get_or_insert_with((objects.clone(), joint_buf.clone()), || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("velocity objects"),
                    layout: &obj_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: objects.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: joint_buf.as_entire_binding(),
                        },
                    ],
                })
            });
        let depth_bg = match depth {
            Some(depth) => {
                let stale = self.camera_depth_bg.as_ref().is_none_or(|(v, _)| v != depth);
                if stale {
                    let layout = self.camera_depth_bgl.as_ref().expect("camera depth bgl");
                    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("camera velocity depth"),
                        layout,
                        entries: &[wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(depth),
                        }],
                    });
                    self.camera_depth_bg = Some((depth.clone(), bg));
                }
                self.camera_depth_bg.as_ref().map(|(_, bg)| bg.clone())
            }
            None => None,
        };
        let view = &self.velocity.as_ref().expect("velocity tex").view;
        let order_view = &self.velocity_order.as_ref().expect("velocity order").0;
        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("velocity"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            // Draw-order keys: the latest draw at a pixel wins, whatever the batch order.
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: order_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if let Some(depth_bg) = depth_bg.as_ref() {
            pass.counted_set_pipeline(
                self.camera_velocity_pipe
                    .as_ref()
                    .expect("camera velocity pipeline"),
            );
            pass.counted_set_bind_group(0, &frame_bg, &[]);
            pass.counted_set_bind_group(1, &obj_bg, &[]);
            pass.counted_set_bind_group(2, depth_bg, &[]);
            pass.counted_draw(0..3, 0..1);
        }
        pass.counted_set_bind_group(0, &frame_bg, &[]);
        pass.counted_set_bind_group(1, &obj_bg, &[]);
        // `None` while a graph pipeline is bound. The standard pipes differ by cull mode and
        // by skinning: `(skinning, cull_none)`, skinning 0 rigid, 1 skinned, 2 pre-skinned.
        let mut standard: Option<(u8, bool)> = None;
        // Velocity draws of meshes in one arena chunk share its buffers (ADR-508).
        let mut binds = crate::submission::PassBinds::new(self.bind_trims);
        for draw in draws {
            match &draw.graph {
                Some(graph) => {
                    pass.counted_set_pipeline(&graph.pipeline);
                    pass.counted_set_bind_group(2, &graph.material, &[]);
                    if let Some(uv1) = &graph.uv1 {
                        binds.vertex(&mut pass, 1, uv1);
                    }
                    standard = None;
                }
                None if standard != Some((velocity_skinning(draw), draw.cull_none)) => {
                    let pipe = match (velocity_skinning(draw), draw.cull_none) {
                        (0, false) => self.velocity_pipe.as_ref(),
                        (0, true) => self.velocity_pipe_ds.as_ref(),
                        (1, false) => self.velocity_skin_pipe.as_ref(),
                        (1, true) => self.velocity_skin_pipe_ds.as_ref(),
                        (_, false) => self.velocity_preskin_pipe.as_ref(),
                        (_, true) => self.velocity_preskin_pipe_ds.as_ref(),
                    };
                    pass.counted_set_pipeline(pipe.expect("velocity pipeline"));
                    standard = Some((velocity_skinning(draw), draw.cull_none));
                }
                None => {}
            }
            if draw.graph.is_none()
                && let Some(prev) = &draw.prev_vb
            {
                binds.vertex(&mut pass, 1, prev);
            }
            binds.mesh(&mut pass, &draw.vb, &draw.ib);
            let slot = draw.slot;
            pass.counted_draw_indexed(
                draw.first_index..draw.first_index + draw.index_count,
                draw.base_vertex,
                slot..slot + draw.instances,
            );
        }
        drop(pass);
        self.velocity_binds.add(binds.counts());
        Ok(())
    }

    /// The velocity target of the last [`Self::record_velocity`], if one exists.
    ///
    /// The view is a clone. A later pass can load it and draw more motion.
    #[must_use]
    pub fn velocity_view(&self) -> Option<wgpu::TextureView> {
        self.velocity.as_ref().map(|tex| tex.view.clone())
    }

    /// Drops the history and last frame's motion (camera cut): the next resolve starts a
    /// fresh accumulation. Targets stay allocated.
    pub fn invalidate(&mut self) {
        self.history_scale = 0.0;
        self.reactive = None;
        self.last_output = None;
        self.prev_velocity_valid = false;
    }

    /// Scales the history by `scale` on the next [`Self::resolve`]: the HDR target's
    /// pre-exposure changed by that factor since the history was written.
    pub fn set_history_scale(&mut self, scale: f32) {
        self.history_scale = scale;
    }

    /// Gives the next [`Self::resolve`] a reactive mask (red, 0..=1, how much to trust this frame
    /// over the history, ADR-2065). `None` keeps the history's full weight.
    pub fn set_reactive(&mut self, mask: Option<&wgpu::TextureView>) {
        self.reactive = mask.cloned();
    }

    /// Upscales `current` (render resolution) into an output-resolution HDR view.
    ///
    /// `history_valid` is false on the first frame and for a non-temporal upsample.
    /// `depth` is the single-sample scene depth at the render resolution (standard
    /// Z): the motion of the closest surface in each 3×3 neighbourhood is used. `None`
    /// (MSAA) uses each pixel's own motion.
    /// `sharpen` is the motion-adaptive sharpen's strength at full motion (0 = off,
    /// [`MOTION_SHARPEN`]); it applies to the returned image only, never to the history.
    /// The returned view stays valid until the next call at a different size.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        current: &wgpu::TextureView,
        depth: Option<&wgpu::TextureView>,
        render_w: u32,
        render_h: u32,
        output_w: u32,
        output_h: u32,
        history_weight: f32,
        history_valid: bool,
        sharpen: f32,
    ) -> Result<wgpu::TextureView, RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        let output_w = output_w.max(1);
        let output_h = output_h.max(1);
        self.ensure_resolve(device, output_w, output_h)?;
        let reactive = self.reactive.take();
        let dst_index = self.which;
        let src_index = 1 - dst_index;
        let params = TaauParams {
            inv_size: [
                1.0 / output_w as f32,
                1.0 / output_h as f32,
                1.0 / render_w.max(1) as f32,
                1.0 / render_h.max(1) as f32,
            ],
            blend: [
                history_weight.clamp(0.0, MAX_HISTORY_WEIGHT),
                if history_valid { 1.0 } else { 0.0 },
                if depth.is_some() { 1.0 } else { 0.0 },
                if history_valid && self.prev_velocity_valid {
                    1.0
                } else {
                    0.0
                },
            ],
            extra: [
                std::mem::take(&mut self.history_scale),
                if reactive.is_some() { 1.0 } else { 0.0 },
                0.0,
                0.0,
            ],
        };
        belt.write(
            device,
            encoder,
            self.params.as_ref().expect("taau params"),
            0,
            bytemuck::bytes_of(&params),
        );
        let history = self.history[src_index]
            .as_ref()
            .map(|tex| tex.view.clone())
            .or_else(|| self.dummy.as_ref().map(|tex| tex.view.clone()))
            .expect("taau history");
        let velocity = self
            .velocity
            .as_ref()
            .filter(|tex| tex.width == render_w.max(1) && tex.height == render_h.max(1))
            .map(|tex| tex.view.clone())
            .or_else(|| self.dummy.as_ref().map(|tex| tex.view.clone()))
            .expect("taau velocity");
        // Bind the previous frame's velocity whenever it exists at this size, even after a
        // camera cut. The shader already ignores it (`blend.w = history_valid && …`), so a cut
        // must not swap the view for the 1×1 dummy and, with it, rebuild the resolve bind
        // group (gpu-farm4: one `taau` bind group per cut).
        let prev_velocity = self
            .prev_velocity
            .as_ref()
            .filter(|_| self.prev_velocity_valid)
            .map(|tex| tex.view.clone())
            .or_else(|| self.dummy.as_ref().map(|tex| tex.view.clone()))
            .expect("taau previous velocity");
        let reactive = reactive
            .or_else(|| self.dummy.as_ref().map(|tex| tex.view.clone()))
            .expect("taau reactive");
        let output = self.history[dst_index]
            .as_ref()
            .expect("taau output")
            .view
            .clone();
        let layout = self.bgl.clone().expect("taau bgl");
        let sampler = self.sampler.clone().expect("taau sampler");
        let params_buf = self.params.clone().expect("taau params");
        let current_key = current.clone();
        let depth = depth
            .cloned()
            .or_else(|| self.dummy_depth.clone())
            .expect("taau depth");
        let bg = self.resolve_cache.get_or_insert_with(
            (
                current_key,
                history.clone(),
                velocity.clone(),
                depth.clone(),
                prev_velocity.clone(),
                reactive.clone(),
                params_buf.clone(),
            ),
            || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("taau"),
                    layout: &layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(current),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&history),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(&velocity),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::Sampler(&sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: params_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 5,
                            resource: wgpu::BindingResource::TextureView(&depth),
                        },
                        wgpu::BindGroupEntry {
                            binding: 6,
                            resource: wgpu::BindingResource::TextureView(&prev_velocity),
                        },
                        wgpu::BindGroupEntry {
                            binding: 7,
                            resource: wgpu::BindingResource::TextureView(&reactive),
                        },
                    ],
                })
            },
        );
        {
            let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("taau"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &output,
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
            pass.counted_set_pipeline(self.pipeline.as_ref().expect("taau pipeline"));
            pass.counted_set_bind_group(0, &bg, &[]);
            pass.counted_draw(0..3, 0..1);
        }
        self.which = src_index;
        self.last_output = Some((output_w, output_h));
        if sharpen > 0.0 && history_valid {
            return self.record_sharpen(device, belt, encoder, &output, &velocity, sharpen);
        }
        Ok(output)
    }

    /// Records the motion-adaptive sharpen of `resolved` into the display texture.
    fn record_sharpen(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        resolved: &wgpu::TextureView,
        velocity: &wgpu::TextureView,
        sharpen: f32,
    ) -> Result<wgpu::TextureView, RenderError> {
        let _span = tracing::info_span!("taau_sharpen").entered();
        let _crumb = crate::breadcrumbs::frame_pass("taau_sharpen");
        let device = &crate::device::TrackedDevice::observe(device);
        let size = resolved.texture().size();
        self.ensure_sharpen(device, size.width, size.height)?;
        let (Some(pipe), Some(layout), Some(params), Some(display)) = (
            self.sharpen_pipe.clone(),
            self.sharpen_bgl.clone(),
            self.sharpen_params.clone(),
            self.display.as_ref().map(|tex| tex.view.clone()),
        ) else {
            return Ok(resolved.clone());
        };
        let amount: [f32; 4] = [sharpen, MOTION_SHARPEN_FULL_PX, 0.0, 0.0];
        belt.write(device, encoder, &params, 0, bytemuck::bytes_of(&amount));
        let bg = self
            .sharpen_cache
            .get_or_insert_with((resolved.clone(), velocity.clone()), || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("taau sharpen"),
                    layout: &layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(resolved),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(velocity),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: params.as_entire_binding(),
                        },
                    ],
                })
            });
        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("taau sharpen"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &display,
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
        pass.counted_set_pipeline(&pipe);
        pass.counted_set_bind_group(0, &bg, &[]);
        pass.counted_draw(0..3, 0..1);
        drop(pass);
        Ok(display)
    }

    fn ensure_sharpen(&mut self, device: &wgpu::Device, width: u32, height: u32) -> Result<(), RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        if self.sharpen_pipe.is_none() {
            let module = naga_module(
                device,
                "taau sharpen",
                include_str!("../shaders/taau_sharpen.wgsl"),
            )?;
            let unfiltered = |binding: u32| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            };
            let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("taau sharpen bgl"),
                entries: &[
                    unfiltered(0),
                    unfiltered(1),
                    uniform_entry(2, wgpu::ShaderStages::FRAGMENT),
                ],
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("taau sharpen layout"),
                bind_group_layouts: &[Some(&bgl)],
                immediate_size: 0,
            });
            self.sharpen_pipe = Some(device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("taau sharpen"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba16Float,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: crate::pipeline_cache::cache_for(&device).as_deref(),
            }));
            self.sharpen_bgl = Some(bgl);
            self.sharpen_params = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("taau sharpen params"),
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        if self
            .display
            .as_ref()
            .is_none_or(|tex| tex.width != width || tex.height != height)
        {
            self.display = Some(hdr_tex(device, width, height, "taau display"));
            self.sharpen_cache.clear();
        }
        Ok(())
    }

    /// The last [`Self::resolve`]'s output if it was `output_w`×`output_h`, without
    /// resolving again: a converged held frame is shown as it is (bit-stable), not
    /// re-blended with an unchanged input. The next resolve reads it as its history.
    #[must_use]
    pub fn last_output(&self, output_w: u32, output_h: u32) -> Option<wgpu::TextureView> {
        if self.last_output != Some((output_w.max(1), output_h.max(1))) {
            return None;
        }
        self.history[1 - self.which].as_ref().map(|tex| tex.view.clone())
    }

    /// Both history views of size `output_w` x `output_h`: the images a converged held view
    /// may hand the post stack (`last_output`), whichever the resolve wrote last.
    pub(crate) fn history_views(&self, output_w: u32, output_h: u32) -> Vec<wgpu::TextureView> {
        self.history
            .iter()
            .flatten()
            .filter(|tex| tex.width == output_w.max(1) && tex.height == output_h.max(1))
            .map(|tex| tex.view.clone())
            .collect()
    }

    /// The velocity pass's frame and object bind group layouts (created on first use), for
    /// material-graph velocity pipelines that append the material group.
    pub fn velocity_layouts(
        &mut self,
        device: &wgpu::Device,
    ) -> (wgpu::BindGroupLayout, wgpu::BindGroupLayout) {
        if self.velocity_frame_bgl.is_none() {
            self.velocity_frame_bgl =
                Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("velocity frame bgl"),
                    entries: &[
                        uniform_entry(0, wgpu::ShaderStages::VERTEX_FRAGMENT),
                        depth_entry(1),
                    ],
                }));
        }
        if self.velocity_obj_bgl.is_none() {
            self.velocity_obj_bgl = Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("velocity object bgl"),
                // Objects, and the joint palettes of the skinned ones.
                entries: &[storage_entry(0), storage_entry(1)],
            }));
        }
        (
            self.velocity_frame_bgl.clone().expect("velocity frame bgl"),
            self.velocity_obj_bgl.clone().expect("velocity obj bgl"),
        )
    }

    fn ensure_velocity(&mut self, device: &wgpu::Device, width: u32, height: u32) -> Result<(), RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        if self.velocity_pipe.is_none()
            || self.velocity_pipe_ds.is_none()
            || self.velocity_skin_pipe.is_none()
            || self.velocity_skin_pipe_ds.is_none()
            || self.velocity_preskin_pipe.is_none()
            || self.velocity_preskin_pipe_ds.is_none()
        {
            let _ = self.velocity_layouts(device);
            let frame_bgl = self.velocity_frame_bgl.clone().expect("velocity frame bgl");
            let obj_bgl = self.velocity_obj_bgl.clone().expect("velocity obj bgl");
            // Both cull modes, skinned and rigid, together: a double-sided material or a
            // character that appears after warm-up must not compile a pipeline on the frame
            // (Khronos glTF 2.0 § 3.9.2). One set per device when the renderer shares them.
            let key = crate::device_pipelines::pipeline_key(&(
                "velocity pipelines",
                include_str!("../shaders/velocity.wgsl"),
            ));
            let shared = self.shared.clone();
            let ready = shared.as_ref().and_then(|reg| reg.get::<VelocityPipes>(key));
            let pipes = match ready {
                Some(pipes) => pipes,
                None => {
                    let module = naga_module(device, "velocity", include_str!("../shaders/velocity.wgsl"))?;
                    let build = || build_velocity_pipes(device, &module, &frame_bgl, &obj_bgl);
                    match shared.as_ref() {
                        Some(reg) => reg.get_or_create(key, build),
                        None => build(),
                    }
                }
            };
            self.velocity_pipe = Some(pipes.mesh);
            self.ensure_dummy_depth(device);
            self.velocity_pipe_ds = Some(pipes.mesh_ds);
            self.velocity_skin_pipe = Some(pipes.skinned);
            self.velocity_skin_pipe_ds = Some(pipes.skinned_ds);
            self.velocity_preskin_pipe = Some(pipes.preskinned);
            self.velocity_preskin_pipe_ds = Some(pipes.preskinned_ds);
            self.camera_velocity_pipe = Some(pipes.camera);
            self.camera_depth_bgl = Some(pipes.depth_bgl);
            self.velocity_frame = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("velocity frame"),
                size: std::mem::size_of::<VelocityFrame>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        if self
            .velocity
            .as_ref()
            .is_none_or(|tex| tex.width != width || tex.height != height)
        {
            self.velocity = Some(hdr_tex(device, width, height, "velocity"));
        }
        self.prev_velocity_valid = self
            .prev_velocity
            .as_ref()
            .is_some_and(|tex| tex.width == width && tex.height == height);
        if self
            .velocity_order
            .as_ref()
            .is_none_or(|(_, w, h)| *w != width || *h != height)
        {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("velocity order"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.velocity_order = Some((view, width, height));
        }
        Ok(())
    }

    fn ensure_resolve(&mut self, device: &wgpu::Device, width: u32, height: u32) -> Result<(), RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        if self.pipeline.is_none() {
            let module = naga_module(device, "taau", include_str!("../shaders/taau.wgsl"))?;
            self.bgl = Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("taau bgl"),
                entries: &[
                    filterable_tex(0),
                    filterable_tex(1),
                    filterable_tex(2),
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    uniform_entry(4, wgpu::ShaderStages::FRAGMENT),
                    wgpu::BindGroupLayoutEntry {
                        binding: 5,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Depth,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    filterable_tex(6),
                    filterable_tex(7),
                ],
            }));
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("taau layout"),
                bind_group_layouts: &[Some(self.bgl.as_ref().expect("taau bgl"))],
                immediate_size: 0,
            });
            self.sampler = Some(device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("taau"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }));

            self.pipeline = Some(device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("taau"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba16Float,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: crate::pipeline_cache::cache_for(&device).as_deref(),
            }));
            self.params = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("taau params"),
                size: std::mem::size_of::<TaauParams>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.dummy = Some(hdr_tex(device, 1, 1, "taau dummy"));
            self.ensure_dummy_depth(device);
        }
        let stale = self.history[0]
            .as_ref()
            .is_none_or(|tex| tex.width != width || tex.height != height);
        if stale {
            self.history[0] = Some(hdr_tex(device, width, height, "taau history 0"));
            self.history[1] = Some(hdr_tex(device, width, height, "taau history 1"));
            self.which = 0;
            self.last_output = None;
        }
        Ok(())
    }
}

impl TaauPass {
    /// The 1×1 depth bound when a frame has no single-sample scene depth (resolve and the
    /// velocity pass's occlusion test, which its uniform then disables).
    fn ensure_dummy_depth(&mut self, device: &wgpu::Device) {
        if self.dummy_depth.is_some() {
            return;
        }
        let depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("taau dummy depth"),
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
        self.dummy_depth = Some(depth.create_view(&wgpu::TextureViewDescriptor::default()));
    }
}

fn depth_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Depth,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn hdr_tex(device: &wgpu::Device, width: u32, height: u32, label: &str) -> Tex {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        // COPY_SRC: `FrameRenderer::request_hdr_capture` reads the upscaled HDR back.
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    crate::resource_inspector::track(label, &texture);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Tex {
        texture,
        view,
        width,
        height,
    }
}

/// The velocity pipelines of one device: everything `vs_main` / `vs_skinned` / `fs_camera`
/// draws with. Shared per device ([`crate::device_pipelines::DevicePipelines`]) so a texture
/// view's first TAA frame, or a stand-in renderer, compiles none of them on the frame thread.
#[derive(Clone)]
struct VelocityPipes {
    mesh: wgpu::RenderPipeline,
    mesh_ds: wgpu::RenderPipeline,
    skinned: wgpu::RenderPipeline,
    skinned_ds: wgpu::RenderPipeline,
    preskinned: wgpu::RenderPipeline,
    preskinned_ds: wgpu::RenderPipeline,
    camera: wgpu::RenderPipeline,
    depth_bgl: wgpu::BindGroupLayout,
}

fn build_velocity_pipes(
    device: &crate::device::TrackedDevice,
    module: &wgpu::ShaderModule,
    frame_bgl: &wgpu::BindGroupLayout,
    obj_bgl: &wgpu::BindGroupLayout,
) -> VelocityPipes {
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("velocity layout"),
        bind_group_layouts: &[Some(frame_bgl), Some(obj_bgl)],
        immediate_size: 0,
    });
    // Both cull modes, together: a double-sided material that appears after warm-up
    // must not compile a pipeline on the frame (Khronos glTF 2.0 § 3.9.2).
    // Last frame's skinned positions of a compute twin (`vs_preskinned`, location 7): the
    // position of each `LabVertex` in the twin's other buffer.
    let prev_stream = wgpu::VertexBufferLayout {
        array_stride: size_of::<crate::LabVertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &[wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 0,
            shader_location: 7,
        }],
    };
    let mesh_pipe = |label: &'static str, entry: &'static str, cull: Option<wgpu::Face>| {
        let preskinned = entry == "vs_preskinned";
        let streams = [Some(crate::LabVertex::layout()), Some(prev_stream.clone())];
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some(entry),
                buffers: if preskinned { &streams } else { &streams[..1] },
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba16Float,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: cull,
                ..Default::default()
            },
            // `GreaterEqual` on the draw-order key written as fragment depth: equal keys
            // (one object's own triangles) keep the later primitive, as before.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        })
    };
    let mesh = mesh_pipe("velocity", "vs_main", Some(wgpu::Face::Back));
    let mesh_ds = mesh_pipe("velocity cull-none", "vs_main", None);
    // Skinned meshes: the same pass, skinned by both palettes (v0.1.14; a skinned mesh
    // used to write no motion, and TAA/TSR smeared moving characters).
    let skinned = mesh_pipe("velocity skinned", "vs_skinned", Some(wgpu::Face::Back));
    let skinned_ds = mesh_pipe("velocity skinned cull-none", "vs_skinned", None);
    // Compute-skinned twins (Tincal B2): both poses come skinned, nothing is skinned here.
    let preskinned = mesh_pipe("velocity pre-skinned", "vs_preskinned", Some(wgpu::Face::Back));
    let preskinned_ds = mesh_pipe("velocity pre-skinned cull-none", "vs_preskinned", None);
    let depth_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("camera velocity depth bgl"),
        entries: &[depth_entry(0)],
    });
    let camera_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("camera velocity layout"),
        bind_group_layouts: &[Some(frame_bgl), Some(obj_bgl), Some(&depth_bgl)],
        immediate_size: 0,
    });
    let camera = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("camera velocity"),
        layout: Some(&camera_layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_full"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_camera"),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba16Float,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        // The pass carries the draw-order depth: the camera pass writes none of it
        // (order 0), so every moving mesh drawn after it wins its pixels.
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::Always),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(&device).as_deref(),
    });
    VelocityPipes {
        mesh,
        mesh_ds,
        skinned,
        skinned_ds,
        preskinned,
        preskinned_ds,
        camera,
        depth_bgl,
    }
}

/// The standard velocity pipeline a draw needs: 0 rigid, 1 skinned by palettes, 2 a
/// compute-skinned twin.
fn velocity_skinning(draw: &VelocityDraw) -> u8 {
    if draw.prev_vb.is_some() {
        2
    } else {
        u8::from(draw.skinned)
    }
}

fn naga_module(device: &wgpu::Device, label: &str, source: &str) -> Result<wgpu::ShaderModule, RenderError> {
    let module = compose_source(source, label, HashMap::new())?;
    Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
    }))
}

fn uniform_entry(binding: u32, stages: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: stages,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn storage_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn filterable_tex(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "taau.wgsl",
            "TaauParams",
            TaauParams {
                inv_size,
                blend,
                extra
            }
        ),
        crate::contract_row!(
            "velocity.wgsl,velocity_graph.wgsl",
            "VelocityFrame",
            VelocityFrame {
                view_proj,
                unjittered,
                prev_view_proj,
                inv_view_proj,
                occlusion,
                clock,
                clock_live
            }
        ),
        crate::contract_row!(
            "velocity.wgsl,velocity_graph.wgsl",
            "MdiObject",
            VelocityObject {
                model,
                prev_model,
                skin,
                pad
            }
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halton_jitter_stays_inside_half_a_pixel() {
        let (w, h) = (1280u32, 720u32);
        for frame in 1..32 {
            let j = halton_jitter(frame, w, h);
            assert!(j[0].abs() <= 1.0 / w as f32 + 1.0e-6, "x {j:?}");
            assert!(j[1].abs() <= 1.0 / h as f32 + 1.0e-6, "y {j:?}");
        }
        let a = halton_jitter(1, w, h);
        let b = halton_jitter(2, w, h);
        assert!(a != b, "consecutive frames must jitter differently");
        // Halton(2) of sample 1 is 0.5, so the first x jitter is 0.
        assert!(a[0].abs() < 1.0e-6);
    }

    #[test]
    fn jitter_shifts_the_projection_column() {
        let proj = regia_core::glam::Mat4::IDENTITY;
        let jittered = jitter_projection(proj, [0.01, -0.02]);
        assert!((jittered.z_axis.x - (-0.01)).abs() < 1.0e-6);
        assert!((jittered.z_axis.y - 0.02).abs() < 1.0e-6);
    }
}

#[cfg(test)]
#[path = "taau_tests.rs"]
mod gpu_tests;
