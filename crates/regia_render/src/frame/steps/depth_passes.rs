// SPDX-License-Identifier: MPL-2.0
//! The frame's depth passes (ADR-2300): phase 1 of the prepass's two-phase occlusion, the
//! depth prepass and its phase 2, the visibility buffer's cluster cull and geometry pass, and the
//! GPU frustum and Hi-Z cull, with their timestamps. Moved out of `FrameRenderer::render_into`
//! unchanged (the frame's locals are borrowed here, so `&x` became `x`).

use crate::frame::*;

/// What the depth passes read.
pub(in crate::frame) struct DepthPassInputs<'a> {
    /// The scene.
    pub(in crate::frame) scene: &'a FrameScene<'a>,
    /// Visible opaque draws.
    pub(in crate::frame) opaque_ix: &'a [usize],
    /// Material-graph pipeline sets of the frame's draws.
    pub(in crate::frame) graph_sets: &'a shadow_passes::GraphSets,
    /// The instanced depth and shadow lists.
    pub(in crate::frame) instanced: &'a Option<shadow_passes::InstancedPasses>,
    /// The frame's pipeline clones (the depth pipelines).
    pub(in crate::frame) frame_pipes: &'a steps::forward_pipes::FramePipeClones,
    /// The prepass depth.
    pub(in crate::frame) depth_view: &'a wgpu::TextureView,
    /// The multisampled depth, on an MSAA frame.
    pub(in crate::frame) msaa_depth: &'a Option<wgpu::TextureView>,
    /// The frame's view bind group.
    pub(in crate::frame) view_bg: &'a wgpu::BindGroup,
    /// The bindless texture table's group.
    pub(in crate::frame) texture_table_group: &'a Option<wgpu::BindGroup>,
    /// GPU timestamps, when timing is on.
    pub(in crate::frame) timer: &'a Option<(FrameStamps, f32)>,
    /// The culling frustum.
    pub(in crate::frame) frustum: &'a Frustum,
    /// The indirect draw strategy.
    pub(in crate::frame) strategy: crate::gpu_driven::IndirectStrategy,
    /// View-projection without jitter.
    pub(in crate::frame) unjittered_vp: regia_core::glam::Mat4,
    /// Jittered view-projection.
    pub(in crate::frame) view_proj: regia_core::glam::Mat4,
    /// Previous frame's unjittered view-projection.
    pub(in crate::frame) prev_vp: [[f32; 4]; 4],
    /// Vertical field of view.
    pub(in crate::frame) fov_y: f32,
    /// Attachment width.
    pub(in crate::frame) attach_w: u32,
    /// Attachment height.
    pub(in crate::frame) attach_h: u32,
    /// MSAA samples.
    pub(in crate::frame) samples: u32,
    /// Render-submission trims are on.
    pub(in crate::frame) trims: bool,
    /// The device writes timestamps inside passes.
    pub(in crate::frame) in_pass_stamps: bool,
    /// The visibility buffer draws this frame.
    pub(in crate::frame) vis_frame: bool,
    /// The frame renders into a texture view.
    pub(in crate::frame) view_pass: bool,
}

impl FrameRenderer {
    /// Records the depth prepass (two-phase where the GPU culls), the visibility geometry and
    /// the GPU cull; returns whether the visibility buffer drew.
    pub(in crate::frame) fn record_depth_passes(
        &mut self,
        d: &crate::TrackedDevice,
        q: &crate::device::TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        draws: &mut Vec<Draw>,
        counters: prepass::PrepassCounters<'_>,
        inputs: &DepthPassInputs<'_>,
    ) -> Result<bool, RenderError> {
        let &DepthPassInputs {
            scene,
            opaque_ix,
            graph_sets,
            instanced,
            frame_pipes,
            depth_view,
            msaa_depth,
            view_bg,
            texture_table_group,
            timer,
            frustum,
            strategy,
            unjittered_vp,
            view_proj,
            prev_vp,
            fov_y,
            attach_w,
            attach_h,
            samples,
            trims,
            in_pass_stamps,
            vis_frame,
            view_pass,
        } = inputs;
        let prepass::PrepassCounters {
            binds,
            draw_counts,
            shared_depth_stamped,
        } = counters;
        // Each window view tests its own last pyramid and draws from its own cluster history
        // (ADR-7329): the view about to draw brings its pyramid and visibility bits in, the
        // outgoing view's are kept aside. One view never swaps.
        if !view_pass {
            // Views that stopped drawing give their history back (ADR-8620).
            self.retire_idle_occlusion_views();
            self.prepass_occlusion.select_view(&mut self.culler);
            self.visbuffer
                .set_occlusion_view(self.prepass_occlusion.feedback_view);
        }
        // Two-phase occlusion of the depth prepass (ADR-1887): phase 1's cull, against last
        // frame's pyramid, before the prepass draws its survivors. It is the frame graph's
        // `prepass_occlusion` node (ADR-4905): the graph holds it only while two-phase occlusion
        // is on, so with it off nothing is recorded or created and the report says why.
        let cpu = strategy == crate::gpu_driven::IndirectStrategy::Cpu;
        let two_phase = if !self.dispatch_node(BuiltinNode::PrepassOcclusion, !cpu) {
            // What phase 1 reported before it was a graph node: a CPU-culled frame has no GPU
            // objects (switch on or off), a GPU-culled frame without the node has it off.
            if !view_pass {
                self.prepass_occlusion.skip(if cpu {
                    crate::PrepassOcclusionSkip::NoGpuObjects
                } else {
                    crate::PrepassOcclusionSkip::Off
                });
            }
            None
        } else {
            prepass_two_phase::record_phase1(
                prepass_two_phase::TwoPhaseFields {
                    occlusion: &mut self.prepass_occlusion,
                    culler: &mut self.culler,
                    belt: &mut self.belt,
                    meshes: &self.meshes,
                    hiz_occlusion_off: self.hiz_occlusion_off,
                },
                d,
                encoder,
                &prepass_two_phase::TwoPhaseInputs {
                    scene,
                    draws,
                    opaque_ix,
                    instanced,
                    depth_view,
                    attach_w,
                    attach_h,
                    unjittered_vp,
                    frustum,
                    view_pass,
                    samples,
                },
            )?
        };
        // Depth prepass.
        if self.dispatch_node(BuiltinNode::DepthPrepass, scene.depth_prepass) {
            let commands = two_phase.and_then(|_| {
                Some((
                    self.prepass_occlusion.phase1_commands()?.clone(),
                    self.prepass_occlusion.phase2_commands()?.clone(),
                ))
            });
            let prepass_inputs = prepass::PrepassInputs {
                draws,
                opaque_ix,
                graph_sets,
                instanced,
                depth_pipe: &frame_pipes.depth_pipe,
                depth_pipe_ds: &frame_pipes.depth_pipe_ds,
                depth_view,
                msaa_depth,
                view_bg,
                texture_table_group,
                timer,
                samples,
                trims,
                in_pass_stamps,
                two_phase: commands.as_ref().map(|(one, two)| (one, two)),
                depth_base: two_phase.map_or(0, |plan| plan.depth_base),
                eye_mask: self.stereo.mask.as_ref(),
            };
            prepass::record_depth_prepass(
                prepass::PrepassFields {
                    entries: &mut self.entries,
                    materials: &mut self.materials,
                    meshes: &mut self.meshes,
                    objects: &mut self.objects,
                    shared_draw: &mut self.shared_draw,
                },
                encoder,
                &prepass_inputs,
                prepass::PrepassCounters {
                    binds,
                    draw_counts,
                    shared_depth_stamped,
                },
            );
            // Phase 2: the pyramid from phase 1's depth, the cull of what phase 1 skipped, and
            // a second depth pass for the instances it finds visible.
            if let Some(plan) = two_phase
                && plan.phase2
            {
                self.culler
                    .build_hiz(d, encoder, depth_view, attach_w, attach_h)?;
                if let Some(hiz) = self.culler.hiz_view() {
                    self.prepass_occlusion.note_pyramid(
                        hiz,
                        attach_w,
                        attach_h,
                        unjittered_vp.to_cols_array_2d(),
                    );
                }
                self.prepass_occlusion.record_phase2(encoder);
                prepass::record_depth_phase2(
                    prepass::PrepassFields {
                        entries: &mut self.entries,
                        materials: &mut self.materials,
                        meshes: &mut self.meshes,
                        objects: &mut self.objects,
                        shared_draw: &mut self.shared_draw,
                    },
                    encoder,
                    &prepass_inputs,
                    prepass::PrepassCounters {
                        binds,
                        draw_counts,
                        shared_depth_stamped,
                    },
                );
            }
        }
        // The cull's result for `ViewVisibility` (ADR-7255): copied after phase 2, mapped after
        // the submit. Main view only: a texture view never culls in two phases.
        if !view_pass {
            self.record_occlusion_feedback(d, encoder, two_phase.is_some());
        }
        if let Some((set_ts, _)) = timer {
            set_ts.write(encoder, 3);
            if !*shared_depth_stamped {
                set_ts.write(encoder, SHARED_QUERY_BASE);
                set_ts.write(encoder, SHARED_QUERY_BASE + 1);
            }
        }

        // Visibility buffer: cluster cull + visibility pass into the prepass depth, before the
        // forward Hi-Z, SSAO and the forward pass read it.
        let vis_drawn = if !vis_frame || self.visbuffer.instance_count() == 0 {
            false
        } else {
            let eye = regia_core::glam::Vec3::from_array(scene.eye);
            let forward = (regia_core::glam::Vec3::from_array(scene.target) - eye).normalize_or_zero();
            // Masked clusters read their material records and base colors through the arena
            // and the texture table (ADR-3915).
            if self.visbuffer.masked_instance_count() > 0
                && let (Some(arena), Some(table)) =
                    (self.material_arena.as_ref(), texture_table_group.as_ref())
            {
                let materials = &self.materials;
                let stride = arena.stats().stride.max(1);
                self.visbuffer
                    .bind_masked(d, q, arena, table, self.material_texture_bias(), |key| {
                        let offset = materials.get(&key)?.dyn_offset?;
                        u32::try_from(u64::from(offset) / stride).ok()
                    });
            }
            self.visbuffer.record_geometry(
                d,
                q,
                encoder,
                depth_view,
                &crate::visbuffer::VisView {
                    view_proj: view_proj.to_cols_array_2d(),
                    cull_view_proj: unjittered_vp.to_cols_array_2d(),
                    prev_view_proj: prev_vp,
                    eye: scene.eye,
                    forward: forward.to_array(),
                    near: scene.near,
                    fov_y,
                    width: attach_w,
                    height: attach_h,
                    double_sided: scene.double_sided,
                },
            )?
        };

        // The cluster cull's per-instance result for `ViewVisibility` (ADR-7286): its visibility
        // bits, copied after the visibility pass, mapped after the submit. Every window view
        // records into its own ring, from its own bits (ADR-7329); a texture view draws no
        // clusters.
        if !view_pass {
            self.record_cluster_feedback(d, encoder, vis_drawn);
        }

        crate::pipeline_warm::warm_checkpoint()?;
        // GPU frustum + Hi-Z. Depth is already recorded, so the pyramid sees this frame.
        if self.dispatch_node(BuiltinNode::GpuCull, !cpu) {
            let built = gpu_cull::record_gpu_cull(
                gpu_cull::CullFields {
                    culler: &mut self.culler,
                    belt: &mut self.belt,
                    scratch: &mut self.scratch,
                    meshes: &self.meshes,
                    hiz_occlusion_off: self.hiz_occlusion_off,
                },
                d,
                encoder,
                draws,
                &gpu_cull::CullInputs {
                    scene,
                    opaque_ix,
                    depth_view,
                    attach_w,
                    attach_h,
                    unjittered_vp,
                    frustum,
                    view_pass,
                },
            )?;
            // The pyramid now holds this frame's whole depth: next frame's phase 1 tests it.
            if built
                && !view_pass
                && let Some(hiz) = self.culler.hiz_view()
            {
                self.prepass_occlusion.note_pyramid(
                    hiz,
                    attach_w,
                    attach_h,
                    unjittered_vp.to_cols_array_2d(),
                );
            }
        }
        if !view_pass {
            self.prepass_occlusion.report_pyramid(&self.culler);
        }
        // An opaque draw collection sent to the GPU-driven batches that the cull did not take
        // draws per object (`lifecycle`; the v0.1.13 "object entry" panic).
        if !cpu {
            let now = self.frame_index;
            self.draw_cull_misses_per_object(d, q, draws, opaque_ix, now);
        }
        if let Some((set_ts, _)) = timer {
            set_ts.write(encoder, 4);
        }
        // Test hook: the depth as the depth passes left it, before the forward pass draws and
        // writes depth again (`set_prepass_depth_capture`, ADR-5953; here, not in `render_into`,
        // whose line ceiling it passed, ADR-5993).
        #[cfg(feature = "test-hooks")]
        if samples == 1 {
            self.capture_prepass_depth(d, encoder, depth_view, attach_w, attach_h);
        }
        Ok(vis_drawn)
    }
}
