// SPDX-License-Identifier: MPL-2.0
//! The frame recording context of `FrameRenderer::render_into` (ADR-4856).
//!
//! `render_into` kept the results of its opening and modes steps, and the state its passes record
//! through (the encoder, its finished segments, the timer, the stamp flags, the bind, submit and
//! draw counters), as dozens of loose locals. They now live in two values:
//!
//! - [`FrameRecording`], opened after the modes step: the frame clock and readback time
//!   ([`FrameOpen`]), the frame's modes and matrices ([`FrameModes`]) and the shadow setup's CPU
//!   timings. Everything after the modes step reads it.
//! - [`PassRecording`], opened where the frame's encoder is created: what every pass records
//!   into or counts on.
//!
//! This is the first slice of the frame's move to graph-driven built-in passes: the values, the
//! statements and their order are unchanged (the frame records the same commands), so a built-in
//! pass's record callback can later be handed these two instead of a list of locals. The step
//! products between them (draw lists, shadow plan, view group, pipelines, targets, screen reuse)
//! are still locals of `render_into`; they move with their passes.

use crate::frame::*;

use super::begin::FrameOpen;
use super::modes::FrameModes;

/// The frame-wide state of one `render_into` call: its opening and modes, and the shadow setup's
/// CPU timings.
pub(in crate::frame) struct FrameRecording {
    /// The frame clock, the texture-view flag and the CPU time spent in readbacks so far.
    pub(in crate::frame) open: FrameOpen,
    /// The frame's modes and matrices. Holds the bind-cache epoch scope, so the context lives to
    /// the end of `render_into`.
    pub(in crate::frame) modes: FrameModes,
    /// CPU time of the shadow plan, the caster lists and the shadow passes.
    pub(in crate::frame) shadow_cpu: ShadowCpuTimings,
}

impl FrameRecording {
    /// The context of a frame that opened with `open` and runs in `modes`.
    pub(in crate::frame) fn new(open: FrameOpen, modes: FrameModes) -> Self {
        Self {
            open,
            modes,
            shadow_cpu: ShadowCpuTimings::default(),
        }
    }
}

/// What the frame's passes record into and count on, from the frame's encoder to its submit.
pub(in crate::frame) struct PassRecording {
    /// The frame's encoder. It is cut at a few pass boundaries (`parallel_finish`).
    pub(in crate::frame) encoder: wgpu::CommandEncoder,
    /// Finished segments of this frame, in recording order: each segment is finished on a worker
    /// while the next is recorded, and the frame submits once at the end.
    pub(in crate::frame) segments: crate::parallel_finish::Segments,
    /// The GPU timer's stamp handle (ADR-5386) and the queue's timestamp period, when timing is on.
    pub(in crate::frame) timer: Option<(FrameStamps, f32)>,
    /// The device writes timestamps inside passes.
    pub(in crate::frame) in_pass_stamps: bool,
    /// The shared draw's depth pass wrote its in-pass stamps. Every query a frame resolves must be
    /// written: an unwritten in-pass pair falls back to two adjacent encoder stamps (0 ms).
    pub(in crate::frame) shared_depth_stamped: bool,
    /// The shared draw's colour pass wrote its in-pass stamps.
    pub(in crate::frame) shared_color_stamped: bool,
    /// The forward pass's glass run wrote its in-pass stamps.
    pub(in crate::frame) glass_stamped: bool,
    /// Render-submission trims are on (ADR-508): each mesh pass skips the vertex and index binds
    /// it already holds.
    pub(in crate::frame) trims: bool,
    /// The binds the mesh passes made and skipped.
    pub(in crate::frame) binds: crate::submission::BindCounts,
    /// The frame's submission counters, published with its submit.
    pub(in crate::frame) submit_stats: crate::SubmissionStats,
    /// Encoder calls of this frame's draw loops (`DrawTally`, ADR-316): kept for the main view.
    pub(in crate::frame) draw_counts: crate::frame_stats::FrameDrawCounts,
}

impl PassRecording {
    /// Closes forward timing and fills unused shared/glass stamp pairs in the same node.
    pub(in crate::frame) fn finish_forward_timestamps(&mut self) {
        if let Some((set, _)) = &self.timer {
            set.write(&mut self.encoder, 6);
            if !self.shared_color_stamped {
                set.write(&mut self.encoder, super::super::SHARED_QUERY_BASE + 2);
                set.write(&mut self.encoder, super::super::SHARED_QUERY_BASE + 3);
            }
            if !self.glass_stamped {
                set.write(&mut self.encoder, super::super::FORWARD_GLASS_QUERY_BASE);
                set.write(&mut self.encoder, super::super::FORWARD_GLASS_QUERY_BASE + 1);
            }
        }
    }

    /// Creates the frame's encoder (label `frame`) with no finished segment, reads the timer (its
    /// stamps are tagged with the frame dispatch's open node, ADR-5386) and the in-pass stamp
    /// feature, and starts the counters with `trims`. Records nothing.
    pub(in crate::frame) fn begin(
        d: &crate::TrackedDevice,
        q: &crate::device::TrackedQueue,
        gpu_timer: Option<&GpuTimer>,
        trims: bool,
    ) -> Self {
        let encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        Self {
            encoder,
            segments: crate::parallel_finish::Segments::default(),
            timer: gpu_timer.map(|t| (FrameStamps::begin(t), q.get_timestamp_period())),
            in_pass_stamps: d
                .features()
                .contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_PASSES),
            shared_depth_stamped: false,
            shared_color_stamped: false,
            glass_stamped: false,
            trims,
            binds: crate::submission::BindCounts::default(),
            submit_stats: crate::SubmissionStats {
                trims,
                ..crate::SubmissionStats::default()
            },
            draw_counts: crate::frame_stats::FrameDrawCounts::default(),
        }
    }
}

/// A product of an earlier record site of `FrameRenderer::render_into` (ADR-4945). The frame graph
/// hands the frame its sites in node order, so a later site finds it; a site that ran before the
/// site that makes it gets `frame_site_missing_input`, naming the product.
pub(in crate::frame) fn site_input<'a, T>(product: &'a Option<T>, name: &str) -> Result<&'a T, RenderError> {
    product.as_ref().ok_or_else(|| {
        RenderError::Encode(format!(
            "frame_site_missing_input: `{name}` comes from a record site the graph has not run yet"
        ))
    })
}
