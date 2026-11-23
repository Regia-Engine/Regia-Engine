// SPDX-License-Identifier: MPL-2.0
//! The frame's submit (ADR-2300): the texture views' publish copies, the timestamp resolve, the
//! belt upload, the one submit of the frame's finished segments, and the mapping of the timer,
//! high-end and exposure readbacks. Moved out of `FrameRenderer::render_into` unchanged.

use crate::frame::*;

impl FrameRenderer {
    /// Finishes the frame's encoder (the publish copies and the timestamp resolve last), submits
    /// it with the segments finished before it, and maps the readbacks; returns the frame's
    /// submit count and the CPU time in submit (ms).
    pub(in crate::frame) fn submit_frame(
        &mut self,
        q: &crate::device::TrackedQueue,
        mut encoder: wgpu::CommandEncoder,
        segments: crate::parallel_finish::Segments,
    ) -> Result<(u32, f32), RenderError> {
        let mut submit_ms = 0.0f32;
        // A texture view's image into the GPU textures materials bind it as
        // ([`Self::render_texture_view`]): after the frame wrote it, before the next pass reads it.
        for (source, destination) in std::mem::take(&mut self.publish_copies) {
            encoder.copy_texture_to_texture(
                source.as_image_copy(),
                destination.as_image_copy(),
                source.size(),
            );
        }
        // Timestamps resolve last, so the post stack and the display pass are timed too.
        if let Some(index) = self.acquire_timer_slot() {
            let recorded = self.extension_passes.dispatch.frame_recorded_mask();
            if let Some(timer_state) = self.gpu_timer.as_mut() {
                timer_state.write_end(&mut encoder);
                // Which node wrote each stamp travels with the slot to its readback (ADR-5386).
                timer_state.slots[index].nodes = timer_state.log.snapshot(recorded);
                encoder.resolve_query_set(&timer_state.set, 0..TIMESTAMP_QUERIES, &timer_state.resolve, 0);
                encoder.copy_buffer_to_buffer(
                    &timer_state.resolve,
                    0,
                    &timer_state.slots[index].staging,
                    0,
                    u64::from(TIMESTAMP_QUERIES) * 8,
                );
            }
            self.pending_timer_slot = Some(index);
        }
        debug_assert!(
            self.belt.pending_is_empty(),
            "a defer after flush was not copied into the frame encoder"
        );
        self.belt.upload(q);
        let submits_before = crate::frame_stats::submits();
        {
            let _submit = tracing::info_span!("submit").entered();
            let started = std::time::Instant::now();
            let buffers = self.finish_workers.join(segments, encoder)?;
            self.last_submission = Some(q.submit(buffers));
            // The belt's mesh copies were in that submit.
            self.mesh_belt_keys.clear();
            submit_ms += started.elapsed().as_secs_f32() * 1000.0;
        }
        self.belt.after_submit();
        // The feedback copy is in this submit. Map it now, not in its own command buffer.
        self.map_feedback_after_submit();
        // The two-phase occlusion copy too (ADR-7255).
        self.map_occlusion_feedback();
        // The counter, not a local increment: evidence reports TrackedQueue submits.
        let submit_count =
            u32::try_from(crate::frame_stats::submits().saturating_sub(submits_before)).unwrap_or(u32::MAX);
        self.map_timer_slot();
        self.highend.after_submit();
        if let Some(post) = self.post.as_mut() {
            post.map_after_submit();
        }
        Ok((submit_count, submit_ms))
    }
}
