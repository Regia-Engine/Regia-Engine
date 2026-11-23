// SPDX-License-Identifier: MPL-2.0
//! The 2D pane's animation clock (`docs/specs/86-2d-toolkit.md` § 13, ADR-2714).
//!
//! The runtime sprite pass shows an animated tile by `regia_2d::batch::animated_tile` at
//! `Time::elapsed().as_millis()`. The pane keeps its own [`AnimClock`] and calls the same function
//! with the same millisecond conversion, so after the same fixed steps the pane and a running game
//! show the same frame. The clock only moves when it is playing (or when an agent steps it), by an
//! explicit `Duration`: no wall clock inside, so tests and agents are deterministic.

use std::time::Duration;

/// A play/pause animation clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnimClock {
    /// Whether [`AnimClock::tick`] advances the clock.
    pub playing: bool,
    elapsed: Duration,
}

impl AnimClock {
    /// Advances by `dt` unconditionally (an agent step, or one fixed test step).
    pub fn step(&mut self, dt: Duration) {
        self.elapsed = self.elapsed.saturating_add(dt);
    }

    /// Advances by `dt` when playing. Returns true when the clock moved.
    pub fn tick(&mut self, dt: Duration) -> bool {
        if self.playing && !dt.is_zero() {
            self.step(dt);
            true
        } else {
            false
        }
    }

    /// Back to time zero (play state unchanged).
    pub fn reset(&mut self) {
        self.elapsed = Duration::ZERO;
    }

    /// Elapsed time as the sprite pass reads it: whole milliseconds.
    #[must_use]
    pub fn time_ms(&self) -> u64 {
        self.elapsed.as_millis() as u64
    }
}
