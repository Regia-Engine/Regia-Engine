// SPDX-License-Identifier: MPL-2.0
//! Opt-in wall-clock timing of a delivered frame's render (`timings`, ADR-2444).
//!
//! Kept apart from the rest of the render-out path on purpose: that path reads no wall clock, so a
//! job's frames cannot depend on time (`the_render_out_path_reads_no_wall_clock`). The numbers made
//! here only go to the manifest's `render_ms`; nothing reads them back into a frame.

use std::time::Instant;

/// A running timer for one frame's render, or nothing when `timings` is off.
pub(super) struct FrameTimer(Option<Instant>);

impl FrameTimer {
    /// Starts a timer when `on`.
    pub(super) fn start(on: bool) -> Self {
        Self(on.then(Instant::now))
    }

    /// Wall milliseconds since [`Self::start`], or `None` when timing is off.
    pub(super) fn stop_ms(self) -> Option<f64> {
        self.0.map(|started| started.elapsed().as_secs_f64() * 1000.0)
    }
}
