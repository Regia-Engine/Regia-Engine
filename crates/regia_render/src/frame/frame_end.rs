// SPDX-License-Identifier: MPL-2.0
//! The end of `FrameRenderer::render_into` after the submit (ADR-1552 split, line m): the CPU
//! timings and submission counters, the history the next frame reads (view-projection, models,
//! palettes, temporal state), the eviction sweep, the shadow caches' validity and the frame's
//! lists back to the scratch. Moved out of `frame.rs` unchanged.

use super::*;

/// What the end of the frame publishes, keeps for the next frame and returns to the scratch lists.
pub(super) struct FrameEnd {
    /// When the frame started.
    pub(super) frame_started: std::time::Instant,
    /// CPU time in submit (ms).
    pub(super) submit_ms: f32,
    /// CPU time in readbacks and the frame-begin poll (ms).
    pub(super) readback_ms: f32,
    /// Queue submits of the frame.
    pub(super) submit_count: u32,
    /// The shadow plan's CPU timings.
    pub(super) shadow_cpu: ShadowCpuTimings,
    /// The frame rendered into a texture view, not the main view.
    pub(super) view_pass: bool,
    /// Vertex and index buffer binds of the frame's draw loops.
    pub(super) binds: crate::submission::BindCounts,
    /// The frame's submission counters.
    pub(super) submit_stats: crate::SubmissionStats,
    /// Decal receiver stamps drawn this frame.
    pub(super) stamps: Vec<crate::decal_receiver::Stamp>,
    /// The unjittered view-projection (the next frame's previous one).
    pub(super) unjittered_vp: regia_core::glam::Mat4,
    /// The frame's draws (the next frame's previous models and palettes).
    pub(super) draws: Vec<Draw>,
    /// The frame's joint palette.
    pub(super) packed_joints: Vec<JointMatrix>,
    /// Temporal AA/upscale was on.
    pub(super) taau_on: bool,
    /// Output width.
    pub(super) width: u32,
    /// Output height.
    pub(super) height: u32,
    /// The frame clock.
    pub(super) now: u64,
    /// The paged sun cascades are valid after this frame.
    pub(super) commit_csm: bool,
    /// The paged local-light atlas is valid after this frame.
    pub(super) commit_atlas: bool,
    /// Paged shadows were on.
    pub(super) paging: bool,
    /// The local lights uploaded this frame.
    pub(super) uploaded: Vec<crate::LabLocalLight>,
    /// Visible opaque draws.
    pub(super) opaque_ix: Vec<usize>,
    /// Visible blended draws.
    pub(super) blend_ix: Vec<usize>,
    /// Shadow casters.
    pub(super) shadow_ix: Vec<usize>,
    /// Translucent casters.
    pub(super) trans_ix: Vec<usize>,
    /// Sun cascade split distances.
    pub(super) csm_splits: Vec<f32>,
    /// Shadow casters' bounds and cascades.
    pub(super) caster_info: Vec<(Option<crate::paged_shadow::Aabb3>, u8)>,
    /// Translucent casters' bounds and cascades.
    pub(super) trans_info: Vec<(Option<crate::paged_shadow::Aabb3>, u8)>,
    /// Each caster's footprint per cascade.
    pub(super) cascade_uv_lists: scratch::ListOfLists<scratch::Footprint>,
    /// Bounded casters of the page and atlas caches.
    pub(super) casters: Vec<crate::paged_shadow::ShadowCaster>,
    /// `caster_info` index of each entry of `casters`.
    pub(super) caster_slot: Vec<usize>,
    /// The frame's shadow jobs.
    pub(super) shadow_jobs: Vec<ShadowJob>,
}

impl FrameRenderer {
    /// Publishes the frame's timings and counters, keeps its history for the next frame, runs
    /// the eviction sweep and returns the frame's lists to the scratch.
    pub(super) fn finish_frame(&mut self, end: FrameEnd) {
        let FrameEnd {
            frame_started,
            submit_ms,
            readback_ms,
            submit_count,
            shadow_cpu,
            view_pass,
            mut binds,
            mut submit_stats,
            stamps,
            unjittered_vp,
            draws,
            mut packed_joints,
            taau_on,
            width,
            height,
            now,
            commit_csm,
            commit_atlas,
            paging,
            uploaded,
            opaque_ix,
            blend_ix,
            shadow_ix,
            trans_ix,
            csm_splits,
            caster_info,
            trans_info,
            cascade_uv_lists,
            casters,
            caster_slot,
            shadow_jobs,
        } = end;
        let total_ms = frame_started.elapsed().as_secs_f32() * 1000.0;
        self.shadow_cpu = shadow_cpu;
        // Velocity passes count into the frame that recorded them; a texture view's frame is
        // not published (ADR-508).
        let velocity_binds = self.taau_pass.take_velocity_binds();
        if !view_pass {
            binds.add(velocity_binds);
            submit_stats.receiver_stamps = u32::try_from(stamps.len()).unwrap_or(u32::MAX);
            submit_stats.vertex_binds = binds.vertex;
            submit_stats.vertex_binds_skipped = binds.vertex_skipped;
            submit_stats.index_binds = binds.index;
            submit_stats.index_binds_skipped = binds.index_skipped;
            self.submission.publish(submit_stats);
        }
        // The frame-debugger draw log (ADR-8224): a capture frame's records go to its request.
        if let Some(records) = crate::frame_stats::close_draw_log()
            && let Some(copies) = self.extension_passes.pass_copies.as_deref_mut()
        {
            copies.keep_draws(self.frame_index, records);
        }
        self.last_cpu = FrameCpuTimings {
            frame_id: Some(self.frame_index),
            sample_id: super::timing::next_timing_sample_id(),
            encode_ms: (total_ms - submit_ms - readback_ms).max(0.0),
            submit_ms,
            readback_ms,
            present_ms: 0.0,
            total_ms,
            submits: submit_count,
        };

        self.prev_view_proj = unjittered_vp.to_cols_array_2d();
        self.have_prev_vp = true;
        self.prev_models.clear();
        self.prev_palettes.clear();
        for draw in &draws {
            self.prev_models.insert(draw.object, draw.model);
            if draw.joint_count > 0 {
                self.prev_palettes
                    .insert(draw.object, (draw.joint_base, draw.joint_count));
            }
        }
        // This frame's palette is the next frame's previous one (the buffer is reused), and
        // the palette it replaces is the next frame's scratch.
        std::mem::swap(&mut self.prev_joints, &mut packed_joints);
        self.scratch.joints.give(packed_joints);
        self.taau_ready = taau_on;
        if taau_on {
            self.taau_size = (width, height);
        }

        // ---- Eviction sweep ----
        // Every pass is encoded: deferred removals apply now, before the sweep.
        self.close_draw_window(view_pass);
        if !view_pass && (self.cache_trim_pending || self.frame_index.is_multiple_of(EVICT_SWEEP_EVERY)) {
            let pressure = std::mem::take(&mut self.cache_trim_pending);
            self.evict(now, pressure);
        }
        if commit_csm {
            self.csm_valid = true;
        } else if !paging {
            self.csm_valid = false;
        }
        if commit_atlas {
            self.atlas_valid = true;
        } else if !paging {
            self.atlas_valid = false;
        }
        // The frame's lists keep their room for the next frame (ADR-385).
        self.scratch.lights.give(uploaded);
        self.scratch.opaque_ix.give(opaque_ix);
        self.scratch.blend_ix.give(blend_ix);
        self.scratch.shadow_ix.give(shadow_ix);
        self.scratch.trans_ix.give(trans_ix);
        self.scratch.csm_splits.give(csm_splits);
        self.scratch.caster_info.give(caster_info);
        self.scratch.trans_info.give(trans_info);
        self.scratch.cascade_uv.give(cascade_uv_lists);
        self.scratch.casters.give(casters);
        self.scratch.caster_slot.give(caster_slot);
        self.scratch.shadow_jobs.give(shadow_jobs);
        self.scratch.draws.give(draws);
        self.scratch.end_frame(self.culler.scratch_growths());
    }
}
