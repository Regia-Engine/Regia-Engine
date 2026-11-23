// SPDX-License-Identifier: MPL-2.0
//! The velocity pass and the temporal upscale of `FrameRenderer::render_into` (ADR-1552 split by
//! pass): the motion of every opaque draw (instanced by mesh, skinning and cull mode, with
//! depth-derived camera motion for static meshes), the visibility-buffer and terrain velocity,
//! and the one upscaler seam (TAAU, TSR or FSR 3; a plain upsample without TAA). Moved out of
//! `frame.rs` unchanged. Nothing else of the renderer is borrowed here, so this is a `&mut self`
//! method.

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use regia_core::glam::Mat4;

use super::shadow_passes::GraphSets;
use super::{Draw, FrameRenderer, FrameScene, held_history_weight, skin_palettes};
use crate::skin::JointMatrix;
use crate::{RenderDevice, RenderError, TrackedDevice};

/// What the velocity pass and the upscale read from the frame.
pub(super) struct TemporalInputs<'a> {
    /// The frame's scene (eye, double-sided switch, depth-velocity quality).
    pub(super) scene: &'a FrameScene<'a>,
    /// The frame's draws.
    pub(super) draws: &'a [Draw],
    /// Indices of the opaque draws, in draw order.
    pub(super) opaque_ix: &'a [usize],
    /// The material-graph pipeline sets (vertex-stage graphs draw their own motion).
    pub(super) graph_sets: &'a GraphSets,
    /// This frame's packed skin palette.
    pub(super) packed_joints: &'a [JointMatrix],
    /// The shaded HDR image the upscaler reads.
    pub(super) shaded: &'a Option<wgpu::TextureView>,
    /// The single-sample scene depth.
    pub(super) depth_view: &'a wgpu::TextureView,
    /// The frame's timestamp query set, when GPU timing is on.
    pub(super) timer: &'a Option<(super::FrameStamps, f32)>,
    /// The jittered projection times the view.
    pub(super) view_proj: Mat4,
    /// The projection without TAAU jitter times the view.
    pub(super) unjittered_vp: Mat4,
    /// Last frame's unjittered view-projection.
    pub(super) prev_vp: [[f32; 4]; 4],
    /// This frame's sub-pixel jitter.
    pub(super) jitter: [f32; 2],
    /// Output size.
    pub(super) width: u32,
    /// Output size.
    pub(super) height: u32,
    /// Internal (pre-upscale) render size.
    pub(super) internal_w: u32,
    /// Internal (pre-upscale) render size.
    pub(super) internal_h: u32,
    /// MSAA sample count.
    pub(super) samples: u32,
    /// Pre-exposure of the HDR image.
    pub(super) pre_exposure: f32,
    /// The display exposure of the post stack.
    pub(super) post_exposure: f32,
    /// This frame's pre-exposure over last frame's (history rescale).
    pub(super) history_rescale: f32,
    /// The TAAU resolve shows its last output.
    pub(super) taau_frozen: bool,
    /// The scene renders to a linear HDR target.
    pub(super) linear_hdr: bool,
    /// The internal size differs from the output size.
    pub(super) scaled: bool,
    /// TAAU is on this frame.
    pub(super) taau_on: bool,
    /// SGSR upscales this frame instead (no HDR upscale here).
    pub(super) sgsr_on: bool,
    /// Bind trimming is on (submission counters).
    pub(super) trims: bool,
    /// The frame drew visibility-buffer geometry.
    pub(super) vis_drawn: bool,
    /// The reactive mask of the frame's translucent composite (`oit_gpu`, ADR-2065).
    pub(super) reactive: Option<&'a wgpu::TextureView>,
}

impl FrameRenderer {
    /// Records the velocity pass and the temporal upscale (stamp 9) and returns the output-sized
    /// HDR view: the last resolve on a converged held view, this frame's upscale, or `None` when
    /// the frame does not upscale.
    pub(super) fn record_temporal(
        &mut self,
        device: &RenderDevice,
        d: &TrackedDevice,
        encoder: &mut wgpu::CommandEncoder,
        segments: &mut crate::parallel_finish::Segments,
        inputs: &TemporalInputs<'_>,
    ) -> Result<Option<wgpu::TextureView>, RenderError> {
        let TemporalInputs {
            scene,
            draws,
            opaque_ix,
            graph_sets,
            packed_joints,
            shaded,
            depth_view,
            timer,
            view_proj,
            unjittered_vp,
            prev_vp,
            jitter,
            width,
            height,
            internal_w,
            internal_h,
            samples,
            pre_exposure,
            post_exposure,
            history_rescale,
            taau_frozen,
            linear_hdr,
            scaled,
            taau_on,
            sgsr_on,
            trims,
            vis_drawn,
            reactive,
        } = *inputs;
        let mut upscaled: Option<wgpu::TextureView> = None;
        if taau_frozen {
            // A converged held view: its inputs are the cached composite and the outdoor
            // layers of the frames it converged over, so the last resolve is the frame.
            upscaled = self.taau_pass.last_output(width, height);
        } else if linear_hdr
            && (scaled || taau_on)
            && !sgsr_on
            && let Some(current) = shaded.as_ref()
        {
            let history_valid = taau_on && self.taau_ready && self.taau_size == (width, height);
            if taau_on {
                let _span = tracing::info_span!("velocity").entered();
                let _crumb = crate::breadcrumbs::frame_pass("velocity");
                // Every opaque draw, in draw order, then grouped by mesh, skinning and cull
                // mode: one instanced draw per group. Cull-none (a double-sided material, or
                // a frame that already draws both faces) does not join a back-face batch.
                // The pass keeps the latest draw at a pixel (`VelocityInstance::order`), so
                // the motion matches drawing them one by one.
                // `depth_velocity`: static meshes and terrain take camera motion from the
                // depth buffer in one fullscreen pass; only meshes that moved draw.
                // A skinned mesh (v0.1.14) skins its vertices with this frame's palette and
                // with last frame's (`prev_palettes`): a walking character writes the motion of
                // its limbs, not only of its model matrix.
                let depth_velocity = scene.quality.depth_velocity && samples == 1;
                let mut keyed = self.scratch.velocity_keyed.take();
                for &index in opaque_ix {
                    let draw = &draws[index];
                    if self.meshes.contains_key(&draw.mesh) {
                        let cull_none = scene.double_sided || draw.cull_off;
                        let skinned = draw.joint_count > 0;
                        keyed.push((draw.mesh, skinned, cull_none, keyed.len() as u32, index));
                    }
                }
                keyed.sort_unstable_by_key(|&(mesh, skinned, cull_none, order, _)| {
                    (mesh, skinned, cull_none, order)
                });
                let mut instances = self.scratch.velocity_instances.take();
                let mut velocity_joints = self.scratch.velocity_joints.take();
                let mut velocity_draws = self.scratch.velocity_draws.take();
                let mut last_batch: Option<(u64, bool, bool)> = None;
                for &(mesh_key, skinned, cull_none, order, index) in &keyed {
                    let draw = &draws[index];
                    let prev_model = self.prev_models.get(&draw.object).copied().unwrap_or(draw.model);
                    let palettes = skinned.then(|| {
                        skin_palettes(
                            packed_joints,
                            &self.prev_joints,
                            self.prev_palettes.get(&draw.object).copied(),
                            draw.joint_base,
                            draw.joint_count,
                        )
                    });
                    // A vertex-stage graph moves its surface without moving its model (an
                    // animated offset): it keeps drawing its own motion. A skinned mesh draws
                    // while its model or its palette moved.
                    let still = match palettes {
                        Some((current, prev)) => {
                            bytemuck::cast_slice::<_, u8>(current) == bytemuck::cast_slice::<_, u8>(prev)
                        }
                        None => !draw.graph_vertex,
                    };
                    // A morphed copy whose weights changed carries both poses in whole-mesh
                    // copies (ADR-2652); steady weights draw as the rigid mesh they are.
                    let morph_motion = if skinned {
                        None
                    } else {
                        self.morph_motion(mesh_key)
                    };
                    if depth_velocity && prev_model == draw.model && still && morph_motion.is_none() {
                        continue;
                    }
                    // A compute-skinned twin carries both poses in vertex buffers (Tincal B2).
                    let prev_vb = self
                        .skin_twin_prev(mesh_key)
                        .or_else(|| morph_motion.as_ref().map(|(_, prev)| prev.clone()));
                    let skin = palettes.filter(|_| prev_vb.is_none()).map(|(current, prev)| {
                        let base = velocity_joints.len() as u32;
                        velocity_joints.extend_from_slice(current);
                        velocity_joints.extend_from_slice(prev);
                        [base, base + current.len() as u32]
                    });
                    let slot = instances.len() as u32;
                    instances.push(crate::taau::VelocityInstance {
                        model: draw.model,
                        prev_model,
                        skin,
                        order,
                    });
                    // A vertex-stage graph writes the motion of its displaced surface, with
                    // its own pipeline and material group: it never joins a mesh batch. A
                    // skinned mesh writes its skinned motion without the graph's offset.
                    let Some(mesh) = self.meshes.get(&mesh_key) else {
                        continue;
                    };
                    let graph = draw
                        .graph
                        .filter(|_| draw.graph_vertex && !skinned)
                        .and_then(|key| graph_sets.get(&(key, draw.cull_off)))
                        .and_then(|set| {
                            Some(crate::taau::VelocityGraph {
                                pipeline: set.velocity.clone()?,
                                material: self.materials.get(&draw.material)?.bg.clone(),
                                uv1: if set.uv1 { Some(mesh.uv1.clone()?) } else { None },
                            })
                        });
                    if graph.is_none()
                        && last_batch == Some((mesh_key, skinned, cull_none))
                        && let Some(batch) = velocity_draws.last_mut()
                        && batch.graph.is_none()
                    {
                        batch.instances += 1;
                        continue;
                    }
                    last_batch = if graph.is_none() {
                        Some((mesh_key, skinned, cull_none))
                    } else {
                        None
                    };
                    // A morph's two poses are whole meshes from vertex 0 (both slots share the
                    // draw's base vertex).
                    let (vb, base_vertex) = match &morph_motion {
                        Some((current, _)) => (current.clone(), 0),
                        None => (mesh.vb.clone(), mesh.base_vertex),
                    };
                    velocity_draws.push(crate::taau::VelocityDraw {
                        vb,
                        ib: mesh.ib.clone(),
                        index_count: mesh.index_count,
                        first_index: mesh.first_index,
                        base_vertex,
                        slot,
                        instances: 1,
                        cull_none,
                        skinned: skinned && prev_vb.is_none(),
                        graph,
                        prev_vb,
                    });
                }
                {
                    let taau = &mut self.taau_pass;
                    taau.share_pipelines(device.shared_pipelines());
                    taau.set_bind_trims(trims);
                    taau.set_graph_clock(
                        super::gpu_types::graph_clock_row(
                            self.material_time.unwrap_or((0.0, 0.0)),
                            self.foliage_wind.unwrap_or([0.0, 0.0]),
                        ),
                        self.material_time.is_some() || self.foliage_wind.is_some(),
                    );
                    let belt = &mut self.belt;
                    taau.record_velocity(
                        d,
                        belt,
                        encoder,
                        internal_w,
                        internal_h,
                        view_proj.to_cols_array_2d(),
                        unjittered_vp.to_cols_array_2d(),
                        prev_vp,
                        &instances,
                        &velocity_joints,
                        &velocity_draws,
                        depth_velocity.then_some(depth_view),
                        // Moving meshes behind the visible surface write no motion there.
                        (samples == 1).then_some(depth_view),
                    )?;
                }
                // The lists keep their room for the next frame (ADR-385); the draws' buffer
                // handles are dropped now, not held until the next frame's velocity pass.
                velocity_draws.clear();
                self.scratch.velocity_keyed.give(keyed);
                self.scratch.velocity_instances.give(instances);
                self.scratch.velocity_joints.give(velocity_joints);
                self.scratch.velocity_draws.give(velocity_draws);
                if vis_drawn && let Some(view) = self.taau_pass.velocity_view() {
                    self.visbuffer.record_velocity(d, encoder, &view, depth_view);
                }
                if samples == 1
                    && !depth_velocity
                    && let Some(view) = self.taau_pass.velocity_view()
                    && let Some(draw) = self.shared_draw.as_ref()
                {
                    let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("terrain velocity"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        // The forward pass already stored the closest surface.
                        // Terrain only replaces motion where it is that surface,
                        // so a closer mesh keeps the velocity it wrote.
                        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                            view: depth_view,
                            depth_ops: Some(wgpu::Operations {
                                load: wgpu::LoadOp::Load,
                                store: wgpu::StoreOp::Store,
                            }),
                            stencil_ops: None,
                        }),
                        timestamp_writes: None,
                        ..Default::default()
                    });
                    draw.draw(&mut pass, crate::SharedPass::Velocity);
                }
            }
            // One seam for the three upscalers (ADR-324): the profile's upscaler on a
            // single-sample TAA frame, TAAU's resolve (a plain upsample without TAA) otherwise.
            let backend_kind = crate::upscaler::resolve_backend(self.temporal_upscaler, taau_on, samples);
            let velocity = self.taau_pass.velocity_view();
            // The stochastic SSR pass's input and output: whichever this frame did not hand on
            // (asleep or awake), a later frame may (ADR-8122).
            let reflection_views = self.highend.reflection_views();
            let inputs = crate::upscaler::UpscaleInputs {
                color: current,
                // Closest-depth motion needs the single-sample scene depth.
                depth: (samples == 1 && taau_on).then_some(depth_view),
                motion: velocity.as_ref(),
                // The input is pre-exposed by `pre_exposure`; TSR's tonemapped domain
                // wants the display exposure of the radiance it holds.
                exposure: post_exposure / pre_exposure,
                reactive,
                transparency: None,
                jitter,
                reset: !history_valid,
                render_size: (internal_w, internal_h),
                display_size: (width, height),
                camera: crate::upscaler::UpscaleCamera {
                    eye: scene.eye,
                    view_proj,
                    unjittered_view_proj: unjittered_vp,
                    velocity_prev_view_proj: regia_core::glam::Mat4::from_cols_array_2d(&prev_vp),
                },
                history_scale: history_rescale,
                history_weight: if taau_on {
                    held_history_weight(self.screen_hold)
                } else {
                    0.0
                },
                sharpen: if taau_on { crate::taau::MOTION_SHARPEN } else { 0.0 },
                alternates: [reflection_views[0].as_ref(), reflection_views[1].as_ref()],
            };
            #[cfg(feature = "frame-gen")]
            self.frame_gen_note_masks(inputs.reactive, inputs.transparency);
            // A host-selected vendor upscaler (ADR-2002) on a single-sample TAA frame. Any refusal
            // or failure leaves this frame to the built-in upscaler below.
            if taau_on && samples == 1 && self.vendor_upscaler.selected().is_some() {
                let frame = crate::upscaler::plugin::UpscaleFrame {
                    color: inputs.color,
                    depth: inputs.depth,
                    motion: inputs.motion,
                    reactive: inputs.reactive,
                    transparency: inputs.transparency,
                    params: crate::upscaler::plugin::UpscaleParams {
                        jitter: inputs.jitter,
                        reset: inputs.reset,
                        exposure: inputs.exposure,
                        history_scale: inputs.history_scale,
                        ..crate::upscaler::plugin::UpscaleParams::new(inputs.render_size, inputs.display_size)
                    },
                };
                let quality =
                    crate::upscaler::UpscaleQuality::nearest(inputs.render_size, inputs.display_size);
                let mut finished = Vec::new();
                crate::parallel_finish::pass_cuts::flush(encoder, segments);
                upscaled =
                    self.vendor_upscaler
                        .upscale(d, &device.queue, encoder, &mut finished, &frame, quality);
                if !finished.is_empty() {
                    crate::parallel_finish::pass_cuts::flush(encoder, segments);
                    crate::parallel_finish::pass_cuts::boundary(encoder);
                }
                for buffer in finished {
                    segments.push_finished(buffer);
                }
            }
            if upscaled.is_none() {
                let mut ctx = crate::upscaler::UpscaleContext {
                    device: d,
                    shared: device.shared_pipelines(),
                    belt: &mut self.belt,
                };
                let backend = crate::upscaler::select(
                    backend_kind,
                    &mut self.taau_pass,
                    &mut self.tsr_pass,
                    &mut self.fsr3_pass,
                );
                upscaled = Some(backend.upscale(&mut ctx, encoder, &inputs)?);
            }
        }
        if let Some((set_ts, _)) = timer {
            set_ts.write(encoder, 9);
        }
        #[cfg(feature = "frame-gen")]
        self.frame_gen_note_temporal(inputs);
        Ok(upscaled)
    }
}
