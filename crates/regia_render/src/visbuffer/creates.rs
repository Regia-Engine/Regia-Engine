// SPDX-License-Identifier: MPL-2.0
//! What the visibility-buffer path's frame-path device calls created (ADR-355).
//!
//! Every GPU entry point of [`super::gpu::VisBufferPass`] that the renderer calls (prepare, the
//! geometry, velocity and shadow-cull recordings, the resolve pipelines, the double-sided
//! prewarm) reads the thread's frame-path counters (`crate::frame_stats`) before and after its
//! body and charges the difference here. Those counters are bumped by
//! [`crate::device::TrackedDevice`], so every pipeline, bind group, buffer and texture the path
//! makes through it is seen, with no counting code at the create sites. The ledger keeps the
//! running total and the total at the start of the current frame, so the last frame's creates
//! are a difference too.

use super::{VisBufferCreateStats, VisBufferCreates};
use crate::frame_stats::FramePathCounters;

impl VisBufferCreates {
    /// The creates and uploads in a frame-path counter difference.
    fn from_counters(c: &FramePathCounters) -> Self {
        Self {
            pipelines: c.pipelines,
            bind_groups: c.bind_groups,
            buffers: c.buffers,
            buffer_bytes: c.buffer_bytes,
            textures: c.textures,
            texture_bytes: c.texture_bytes,
            upload_bytes: c.upload_bytes,
        }
    }

    /// Pipelines, bind groups, buffers and textures (uploads are not objects).
    #[must_use]
    pub fn objects(&self) -> u64 {
        self.pipelines + self.bind_groups + self.buffers + self.textures
    }

    /// `self − earlier`, per counter.
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            pipelines: self.pipelines.saturating_sub(earlier.pipelines),
            bind_groups: self.bind_groups.saturating_sub(earlier.bind_groups),
            buffers: self.buffers.saturating_sub(earlier.buffers),
            buffer_bytes: self.buffer_bytes.saturating_sub(earlier.buffer_bytes),
            textures: self.textures.saturating_sub(earlier.textures),
            texture_bytes: self.texture_bytes.saturating_sub(earlier.texture_bytes),
            upload_bytes: self.upload_bytes.saturating_sub(earlier.upload_bytes),
        }
    }

    fn add(&mut self, other: &Self) {
        self.pipelines = self.pipelines.saturating_add(other.pipelines);
        self.bind_groups = self.bind_groups.saturating_add(other.bind_groups);
        self.buffers = self.buffers.saturating_add(other.buffers);
        self.buffer_bytes = self.buffer_bytes.saturating_add(other.buffer_bytes);
        self.textures = self.textures.saturating_add(other.textures);
        self.texture_bytes = self.texture_bytes.saturating_add(other.texture_bytes);
        self.upload_bytes = self.upload_bytes.saturating_add(other.upload_bytes);
    }
}

/// Running totals of the path's creates, and where the current frame started.
#[derive(Default)]
pub(crate) struct CreateLedger {
    total: VisBufferCreates,
    /// `total` when the current frame began.
    frame_start: VisBufferCreates,
    /// A frame has begun (the one in progress, or the one that just finished).
    open: bool,
    /// Frames finished before the current one.
    closed: u64,
    /// Of those, the run of frames with no object created, ending with the last closed one.
    quiet: u64,
}

impl CreateLedger {
    /// The frame-path counters of this thread, to hand back to [`Self::charge`].
    pub(crate) fn mark() -> FramePathCounters {
        crate::frame_stats::counters()
    }

    /// Charges what this thread created since `before` (from [`Self::mark`]).
    pub(crate) fn charge(&mut self, before: FramePathCounters) {
        self.add(&VisBufferCreates::from_counters(
            &crate::frame_stats::counters().since(before),
        ));
    }

    fn add(&mut self, delta: &VisBufferCreates) {
        self.total.add(delta);
    }

    /// Closes the previous frame (if one began) and starts the next.
    pub(crate) fn begin_frame(&mut self) {
        if self.open {
            let last = self.total.since(self.frame_start);
            self.closed += 1;
            self.quiet = if last.objects() == 0 { self.quiet + 1 } else { 0 };
        }
        self.frame_start = self.total;
        self.open = true;
    }

    /// The totals, and the frame begun last counted as finished: between two frames that is
    /// the frame the renderer just drew.
    pub(crate) fn stats(&self) -> VisBufferCreateStats {
        if !self.open {
            return VisBufferCreateStats {
                total: self.total,
                ..VisBufferCreateStats::default()
            };
        }
        let last_frame = self.total.since(self.frame_start);
        let frames = self.closed + 1;
        let quiet_frames = if last_frame.objects() == 0 {
            self.quiet + 1
        } else {
            0
        };
        VisBufferCreateStats {
            total: self.total,
            last_frame,
            frames,
            quiet_frames,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(buffers: u64, bind_groups: u64, upload_bytes: u64) -> VisBufferCreates {
        VisBufferCreates {
            buffers,
            buffer_bytes: buffers * 256,
            bind_groups,
            upload_bytes,
            ..VisBufferCreates::default()
        }
    }

    /// The last frame is the one begun last; quiet frames count the run of frames with no
    /// object created, and uploads do not break the run.
    #[test]
    fn frames_and_quiet_runs_follow_the_charges() {
        let mut ledger = CreateLedger::default();
        assert_eq!(ledger.stats(), VisBufferCreateStats::default(), "no frame yet");

        // Frame 0: the warm-up creates.
        ledger.begin_frame();
        ledger.add(&delta(9, 6, 4096));
        let s = ledger.stats();
        assert_eq!((s.frames, s.quiet_frames), (1, 0));
        assert_eq!(s.last_frame, delta(9, 6, 4096));
        assert_eq!(s.total, delta(9, 6, 4096));

        // Frames 1 to 3: uploads only.
        for frame in 1..4u64 {
            ledger.begin_frame();
            ledger.add(&delta(0, 0, 512));
            let s = ledger.stats();
            assert_eq!(s.frames, frame + 1);
            assert_eq!(s.quiet_frames, frame, "frame {frame}");
            assert_eq!(s.last_frame.objects(), 0);
            assert_eq!(s.last_frame.upload_bytes, 512);
        }

        // Frame 4: a growth (one buffer, its bind groups) ends the run.
        ledger.begin_frame();
        ledger.add(&delta(1, 3, 0));
        let s = ledger.stats();
        assert_eq!((s.frames, s.quiet_frames), (5, 0));
        assert_eq!(s.last_frame.objects(), 4);
        assert_eq!(s.total.buffers, 10);
        assert_eq!(s.total.bind_groups, 9);

        // Frame 5: quiet again; the run restarts at 1.
        ledger.begin_frame();
        let s = ledger.stats();
        assert_eq!((s.frames, s.quiet_frames), (6, 1));
        assert_eq!(s.last_frame, VisBufferCreates::default());
    }

    /// Charges between frames (a prewarm, a pipeline built before the first frame) count in
    /// the total and in the frame begun last.
    #[test]
    fn charges_before_the_first_frame_count_in_the_total_only() {
        let mut ledger = CreateLedger::default();
        ledger.add(&VisBufferCreates {
            pipelines: 4,
            ..VisBufferCreates::default()
        });
        let s = ledger.stats();
        assert_eq!(s.total.pipelines, 4);
        assert_eq!((s.frames, s.quiet_frames), (0, 0));
        assert_eq!(s.last_frame, VisBufferCreates::default(), "no frame has begun");
        ledger.begin_frame();
        let s = ledger.stats();
        assert_eq!(s.last_frame.objects(), 0, "the prewarm was before this frame");
        assert_eq!((s.frames, s.quiet_frames), (1, 1));
    }

    /// `charge` reads this thread's frame-path counters: a tracked create between `mark` and
    /// `charge` is charged, and one before `mark` is not.
    #[test]
    fn charge_reads_the_tracked_counters() {
        let mut ledger = CreateLedger::default();
        crate::frame_stats::note_buffer(Some("before the mark"), 64);
        let before = CreateLedger::mark();
        crate::frame_stats::note_buffer(Some("vis test"), 128);
        crate::frame_stats::note_bind_group(Some("vis test"));
        crate::frame_stats::note_upload(32);
        ledger.charge(before);
        let total = ledger.stats().total;
        assert_eq!(
            (
                total.buffers,
                total.buffer_bytes,
                total.bind_groups,
                total.upload_bytes
            ),
            (1, 128, 1, 32)
        );
        assert_eq!((total.pipelines, total.textures), (0, 0));
    }

    #[test]
    fn since_saturates_and_objects_skip_uploads() {
        let a = delta(2, 1, 100);
        assert_eq!(a.since(delta(5, 0, 0)).buffers, 0);
        assert_eq!(a.objects(), 3);
        assert_eq!(delta(0, 0, 1 << 20).objects(), 0);
    }
}
