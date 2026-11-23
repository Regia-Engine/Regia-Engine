// SPDX-License-Identifier: MPL-2.0
//! Present pacing for generated frames (ADR-514 § 4).
//!
//! With frame generation every rendered ("real") frame yields two presents: the frame
//! interpolated between the previous and this real frame, then the real frame itself. The goal is
//! to show each image for the same time. The pacer keeps a moving average of the real frame
//! interval (spikes clamped so one hitch does not throw the estimate), presents the generated
//! frame as soon as both it and the half interval since the last present are due, and the real
//! frame half an interval later. This follows the behaviour the FidelityFX SDK v1.1.4 documents for
//! its frame interpolation swap chain (`docs/techniques/frame-interpolation-swap-chain.md`, MIT):
//! a moving average of several frames, the generated frame first, then the real frame after the
//! target delta. The code is Regia Engine's own: it takes timestamps from the caller and holds no
//! clock, so tests drive it with fixed steps.

use std::collections::VecDeque;

/// Tuning of the [`FramePacer`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PacingConfig {
    /// Real frame intervals in the moving average.
    pub window: usize,
    /// Intervals needed before the first generated frame.
    pub min_samples: usize,
    /// An interval longer than this multiple of the average counts as this multiple.
    pub spike_factor: f32,
    /// Below this real frame rate the pacer presents real frames only.
    pub min_base_fps: f32,
    /// Real frames to hold off after generation dropped under the floor. Doubles on each drop, up
    /// to [`Self::retry_frames_max`]: on a vsync swap chain two presents per tick halve the real
    /// frame rate, which can itself cross the floor, and generation must not flap with it.
    pub retry_frames: u32,
    /// The longest hold-off, in real frames.
    pub retry_frames_max: u32,
}

impl Default for PacingConfig {
    fn default() -> Self {
        Self {
            window: 8,
            min_samples: 2,
            spike_factor: 2.0,
            min_base_fps: 55.0,
            retry_frames: 120,
            retry_frames_max: 3840,
        }
    }
}

/// Why a real frame is presented without a generated frame before it. The codes are part of the
/// agent protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacingSkip {
    /// Frame generation is off.
    Disabled,
    /// Not enough real frames measured yet (start, or after [`FramePacer::reset_history`]).
    Warmup,
    /// A camera cut or history reset: there is nothing to interpolate from.
    Reset,
    /// The average real frame rate is under [`PacingConfig::min_base_fps`].
    BaseRateTooLow,
}

impl PacingSkip {
    /// The protocol code (lowercase ASCII and `_`).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Warmup => "warmup",
            Self::Reset => "reset",
            Self::BaseRateTooLow => "base_rate_too_low",
        }
    }
}

/// When to present, for one real frame. Times are on the caller's clock, in nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingDecision {
    /// Present the generated frame at `generated_at_ns`, then the real frame at `real_at_ns`.
    Generate {
        /// Target time of the generated (interpolated) present.
        generated_at_ns: u64,
        /// Target time of the real present.
        real_at_ns: u64,
        /// The target delta between presents: half the average real frame interval.
        interval_ns: u64,
    },
    /// Present the real frame only.
    RealOnly {
        /// Target time of the real present.
        present_at_ns: u64,
        /// Why no generated frame comes first.
        reason: PacingSkip,
    },
}

/// Counters since the pacer was created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacingStats {
    /// Real frames seen.
    pub real_frames: u64,
    /// Generated frames scheduled.
    pub generated: u64,
    /// Real-only presents because frame generation was off.
    pub skipped_disabled: u64,
    /// Real-only presents during warm-up.
    pub skipped_warmup: u64,
    /// Real-only presents on a reset.
    pub skipped_reset: u64,
    /// Real-only presents because the base rate was too low.
    pub skipped_base_rate: u64,
}

/// The frame pacer: one per presented window.
///
/// `clone_from` reuses the target's interval history, so the frame-generation present path can
/// keep a copy of the pacer every tick without allocating (rev-56 shader-present finding 1).
#[derive(Debug, Default)]
pub struct FramePacer {
    /// Tuning; may change between frames.
    pub config: PacingConfig,
    intervals: VecDeque<u64>,
    last_ready_ns: Option<u64>,
    last_present_ns: Option<u64>,
    stats: PacingStats,
    /// The last frame generated.
    generating: bool,
    /// Frames generated in a row.
    generated_run: u32,
    /// The current hold-off length (0 = none yet).
    hold_off: u32,
    /// Frames left in the hold-off.
    hold_left: u32,
}

impl Clone for FramePacer {
    fn clone(&self) -> Self {
        let mut pacer = Self::default();
        pacer.clone_from(self);
        pacer
    }

    fn clone_from(&mut self, source: &Self) {
        // Every field by name, so a new one cannot be left out.
        let Self {
            config,
            intervals,
            last_ready_ns,
            last_present_ns,
            stats,
            generating,
            generated_run,
            hold_off,
            hold_left,
        } = source;
        self.config = *config;
        self.intervals.clone_from(intervals);
        self.last_ready_ns = *last_ready_ns;
        self.last_present_ns = *last_present_ns;
        self.stats = *stats;
        self.generating = *generating;
        self.generated_run = *generated_run;
        self.hold_off = *hold_off;
        self.hold_left = *hold_left;
    }
}

impl FramePacer {
    /// A pacer with `config`.
    #[must_use]
    pub fn new(config: PacingConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// The moving average of the real frame interval, once one interval is measured.
    #[must_use]
    pub fn average_interval_ns(&self) -> Option<u64> {
        let n = self.intervals.len() as u64;
        (n > 0).then(|| self.intervals.iter().sum::<u64>() / n)
    }

    /// The real frame rate the average implies.
    #[must_use]
    pub fn base_fps(&self) -> Option<f32> {
        self.average_interval_ns()
            .filter(|&ns| ns > 0)
            .map(|ns| (1.0e9 / ns as f64) as f32)
    }

    /// How much later each real frame reaches the screen with frame generation: half an
    /// average interval (the real frame waits behind the generated one). The interpolation's GPU
    /// time comes on top.
    #[must_use]
    pub fn added_latency_ns(&self) -> Option<u64> {
        self.average_interval_ns().map(|ns| ns / 2)
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> PacingStats {
        self.stats
    }

    /// Forgets the measured intervals (a resize, a device change or a long pause); the next
    /// frames warm up again. The counters stay.
    pub fn reset_history(&mut self) {
        self.intervals.clear();
        self.last_ready_ns = None;
    }

    /// One real frame finished rendering at `ready_ns`. `reset` marks a camera cut or a history
    /// reset; `enabled` is the player's choice. Returns when to present.
    pub fn on_real_frame(&mut self, ready_ns: u64, reset: bool, enabled: bool) -> PacingDecision {
        self.stats.real_frames += 1;
        if let Some(last) = self.last_ready_ns
            && ready_ns > last
        {
            let interval = ready_ns - last;
            let clamped = match self.average_interval_ns() {
                Some(avg) => interval.min((avg as f64 * f64::from(self.config.spike_factor.max(1.0))) as u64),
                None => interval,
            };
            self.intervals.push_back(clamped);
            while self.intervals.len() > self.config.window.max(1) {
                self.intervals.pop_front();
            }
        }
        self.last_ready_ns = Some(ready_ns.max(self.last_ready_ns.unwrap_or(0)));

        let average = self.average_interval_ns();
        let max_interval_ns = 1.0e9 / f64::from(self.config.min_base_fps.max(1.0));
        let too_slow = average.is_none_or(|avg| avg as f64 > max_interval_ns);
        let skip = if !enabled {
            Some(PacingSkip::Disabled)
        } else if reset {
            Some(PacingSkip::Reset)
        } else if self.intervals.len() < self.config.min_samples.max(1) {
            Some(PacingSkip::Warmup)
        } else if too_slow || self.hold_left > 0 {
            Some(PacingSkip::BaseRateTooLow)
        } else {
            None
        };
        if skip == Some(PacingSkip::BaseRateTooLow) {
            if self.generating {
                // Generation just crossed the floor: stay off for a while, longer each time.
                self.hold_off = (self.hold_off * 2)
                    .max(self.config.retry_frames)
                    .min(self.config.retry_frames_max);
                self.hold_left = self.hold_off;
            }
            self.hold_left = self.hold_left.saturating_sub(1);
        }
        if skip.is_none() {
            self.generated_run = self.generated_run.saturating_add(1);
            // Running for as long as it was held off means the last drop was a hitch, not two
            // presents crossing the floor by themselves: the next drop starts over.
            if self.generated_run >= self.hold_off {
                self.hold_off = 0;
            }
        } else {
            self.generated_run = 0;
        }
        self.generating = skip.is_none();

        match (skip, average) {
            (None, Some(avg)) => {
                let half = avg / 2;
                let generated_at_ns = self
                    .last_present_ns
                    .map_or(ready_ns, |last| ready_ns.max(last + half));
                let real_at_ns = generated_at_ns + half;
                self.last_present_ns = Some(real_at_ns);
                self.stats.generated += 1;
                PacingDecision::Generate {
                    generated_at_ns,
                    real_at_ns,
                    interval_ns: half,
                }
            }
            (reason, _) => {
                let reason = reason.unwrap_or(PacingSkip::Warmup);
                let counter = match reason {
                    PacingSkip::Disabled => &mut self.stats.skipped_disabled,
                    PacingSkip::Warmup => &mut self.stats.skipped_warmup,
                    PacingSkip::Reset => &mut self.stats.skipped_reset,
                    PacingSkip::BaseRateTooLow => &mut self.stats.skipped_base_rate,
                };
                *counter += 1;
                let present_at_ns = self.last_present_ns.map_or(ready_ns, |last| ready_ns.max(last));
                self.last_present_ns = Some(present_at_ns);
                PacingDecision::RealOnly {
                    present_at_ns,
                    reason,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 60 fps in whole nanoseconds.
    const STEP: u64 = 16_666_666;

    /// Every present time of `decisions`, in order.
    fn presents(decisions: &[PacingDecision]) -> Vec<u64> {
        let mut out = Vec::new();
        for d in decisions {
            match *d {
                PacingDecision::Generate {
                    generated_at_ns,
                    real_at_ns,
                    ..
                } => {
                    out.push(generated_at_ns);
                    out.push(real_at_ns);
                }
                PacingDecision::RealOnly { present_at_ns, .. } => out.push(present_at_ns),
            }
        }
        out
    }

    #[test]
    fn a_steady_frame_rate_presents_at_even_half_intervals() {
        let mut pacer = FramePacer::default();
        let decisions: Vec<_> = (0..20)
            .map(|i| pacer.on_real_frame(i * STEP, false, true))
            .collect();
        // Frame 0 has no interval, frame 1 has one: both warm up.
        for d in &decisions[..2] {
            assert!(
                matches!(
                    d,
                    PacingDecision::RealOnly {
                        reason: PacingSkip::Warmup,
                        ..
                    }
                ),
                "{d:?}"
            );
        }
        let times = presents(&decisions[2..]);
        assert_eq!(times.len(), 36);
        for pair in times.windows(2) {
            assert_eq!(pair[1] - pair[0], STEP / 2, "{times:?}");
        }
        // The generated frame goes out when its real frame is ready; the real one half a frame later.
        for (i, d) in decisions.iter().enumerate().skip(2) {
            let ready = i as u64 * STEP;
            assert_eq!(
                *d,
                PacingDecision::Generate {
                    generated_at_ns: ready,
                    real_at_ns: ready + STEP / 2,
                    interval_ns: STEP / 2
                }
            );
        }
        assert_eq!(pacer.added_latency_ns(), Some(STEP / 2));
        let fps = pacer.base_fps().unwrap_or(0.0);
        assert!((fps - 60.0).abs() < 0.01, "{fps}");
        let stats = pacer.stats();
        assert_eq!(
            (stats.real_frames, stats.generated, stats.skipped_warmup),
            (20, 18, 2)
        );
    }

    #[test]
    fn presents_never_precede_their_frame_or_each_other_under_jitter() {
        let mut pacer = FramePacer::new(PacingConfig {
            min_base_fps: 20.0,
            ..PacingConfig::default()
        });
        // Deterministic uneven intervals between 12 and 21 ms, plus one 80 ms hitch.
        let mut ready = 0u64;
        let mut last_present = 0u64;
        for i in 0..64u64 {
            let step = if i == 30 {
                80_000_000
            } else {
                12_000_000 + (i * 7_919 % 9) * 1_000_000
            };
            ready += step;
            match pacer.on_real_frame(ready, false, true) {
                PacingDecision::Generate {
                    generated_at_ns,
                    real_at_ns,
                    interval_ns,
                } => {
                    assert!(generated_at_ns >= ready);
                    assert!(generated_at_ns >= last_present);
                    assert_eq!(real_at_ns, generated_at_ns + interval_ns);
                    last_present = real_at_ns;
                }
                PacingDecision::RealOnly { present_at_ns, .. } => {
                    assert!(present_at_ns >= ready && present_at_ns >= last_present);
                    last_present = present_at_ns;
                }
            }
            // The real frame never waits more than one average interval behind its ready time
            // (half for the generated present, half for itself), even after the hitch.
            let avg = pacer.average_interval_ns().unwrap_or(0);
            assert!(last_present <= ready + avg.max(80_000_000), "frame {i}");
        }
    }

    #[test]
    fn a_hitch_moves_the_average_by_at_most_the_spike_clamp() {
        let mut pacer = FramePacer::default();
        for i in 0..8 {
            pacer.on_real_frame(i * STEP, false, true);
        }
        let before = pacer.average_interval_ns().unwrap_or(0);
        assert_eq!(before, STEP);
        // A 200 ms hitch counts as 2 × the average.
        pacer.on_real_frame(7 * STEP + 200_000_000, false, true);
        let after = pacer.average_interval_ns().unwrap_or(0);
        assert_eq!(after, (7 * STEP + 2 * STEP) / 8);
    }

    #[test]
    fn the_window_keeps_only_the_latest_intervals() {
        let mut pacer = FramePacer::default();
        let mut t = 0;
        for _ in 0..10 {
            t += 2 * STEP;
            pacer.on_real_frame(t, false, true);
        }
        // Eight quick frames replace every slow interval (each within the spike clamp).
        for _ in 0..8 {
            t += STEP;
            pacer.on_real_frame(t, false, true);
        }
        assert_eq!(pacer.average_interval_ns(), Some(STEP));
    }

    #[test]
    fn reset_disabled_and_low_rate_present_the_real_frame_only() {
        let mut pacer = FramePacer::default();
        for i in 0..4 {
            pacer.on_real_frame(i * STEP, false, true);
        }
        assert_eq!(
            pacer.on_real_frame(4 * STEP, true, true),
            PacingDecision::RealOnly {
                present_at_ns: 4 * STEP,
                reason: PacingSkip::Reset
            }
        );
        // The interval measured across the reset still counts: the next frame generates again.
        assert!(matches!(
            pacer.on_real_frame(5 * STEP, false, true),
            PacingDecision::Generate { .. }
        ));
        assert!(matches!(
            pacer.on_real_frame(6 * STEP, false, false),
            PacingDecision::RealOnly {
                reason: PacingSkip::Disabled,
                ..
            }
        ));
        // 30 fps under the 55 fps floor.
        let mut slow = FramePacer::default();
        let decisions: Vec<_> = (0..6)
            .map(|i| slow.on_real_frame(i * 2 * STEP, false, true))
            .collect();
        assert!(matches!(
            decisions[5],
            PacingDecision::RealOnly {
                reason: PacingSkip::BaseRateTooLow,
                ..
            }
        ));
        let stats = slow.stats();
        assert_eq!(
            (stats.skipped_warmup, stats.skipped_base_rate, stats.generated),
            (2, 4, 0)
        );
        let stats = pacer.stats();
        assert_eq!(
            (stats.skipped_reset, stats.skipped_disabled, stats.generated),
            (1, 1, 3)
        );
    }

    #[test]
    fn reset_history_warms_up_again_and_codes_are_snake_case() {
        let mut pacer = FramePacer::default();
        for i in 0..4 {
            pacer.on_real_frame(i * STEP, false, true);
        }
        pacer.reset_history();
        assert_eq!(pacer.average_interval_ns(), None);
        assert!(matches!(
            pacer.on_real_frame(10 * STEP, false, true),
            PacingDecision::RealOnly {
                reason: PacingSkip::Warmup,
                ..
            }
        ));
        for skip in [
            PacingSkip::Disabled,
            PacingSkip::Warmup,
            PacingSkip::Reset,
            PacingSkip::BaseRateTooLow,
        ] {
            assert!(skip.code().bytes().all(|c| c.is_ascii_lowercase() || c == b'_'));
        }
    }

    /// The real intervals of a vsync swap chain at 60 Hz: 16.7 ms while real frames only, 33.3 ms
    /// while each tick presents twice.
    fn vsync_interval(generated: bool) -> u64 {
        if generated { 2 * STEP } else { STEP }
    }

    #[test]
    fn crossing_the_floor_holds_generation_off_longer_each_time() {
        // The floor sits between the two rates, the case that flaps without the hold-off.
        let mut pacer = FramePacer::new(PacingConfig {
            min_base_fps: 45.0,
            retry_frames: 20,
            retry_frames_max: 80,
            ..PacingConfig::default()
        });
        let mut now = 0;
        let mut generated_last = false;
        let mut runs_off = Vec::new();
        let mut off = 0u32;
        for _ in 0..600 {
            now += vsync_interval(generated_last);
            let decision = pacer.on_real_frame(now, false, true);
            let generate = matches!(decision, PacingDecision::Generate { .. });
            if generate {
                if off > 0 {
                    runs_off.push(off);
                }
                off = 0;
            } else {
                off += 1;
            }
            generated_last = generate;
        }
        // Warm-up first, then hold-offs of 20, 40, 80, 80 (each drop also takes the frames the
        // moving average needs to fall back under the floor).
        assert!(runs_off.len() >= 3, "{runs_off:?}");
        let holds = &runs_off[1..];
        assert!(holds.windows(2).all(|w| w[1] >= w[0]), "{runs_off:?}");
        assert!(holds.iter().all(|&h| h <= 80 + 16), "{runs_off:?}");
        assert!(holds[0] >= 20 && holds[holds.len() - 1] >= 80, "{runs_off:?}");
    }

    #[test]
    fn a_run_as_long_as_the_hold_off_forgets_it() {
        let mut pacer = FramePacer::new(PacingConfig {
            min_base_fps: 45.0,
            retry_frames: 10,
            retry_frames_max: 80,
            ..PacingConfig::default()
        });
        let mut now = 0;
        let run = |pacer: &mut FramePacer, now: &mut u64, frames: u32, step: u64| -> u32 {
            let mut skipped = 0;
            for _ in 0..frames {
                *now += step;
                if !matches!(
                    pacer.on_real_frame(*now, false, true),
                    PacingDecision::Generate { .. }
                ) {
                    skipped += 1;
                }
            }
            skipped
        };
        // A long run, then slow patches with long runs between them: every hold-off has the first
        // length, where drops close together would double it.
        run(&mut pacer, &mut now, 100, STEP);
        let mut holds = Vec::new();
        for _ in 0..3 {
            run(&mut pacer, &mut now, 20, 4 * STEP);
            holds.push(run(&mut pacer, &mut now, 40, STEP));
            assert_eq!(run(&mut pacer, &mut now, 40, STEP), 0, "generating again");
        }
        assert!(holds[0] >= 1, "{holds:?}");
        assert_eq!(holds[0], holds[1], "{holds:?}");
        assert_eq!(holds[1], holds[2], "{holds:?}");
    }

    #[test]
    fn a_rate_well_over_the_floor_never_holds_off() {
        let mut pacer = FramePacer::new(PacingConfig {
            min_base_fps: 30.0,
            ..PacingConfig::default()
        });
        for i in 0..400u64 {
            pacer.on_real_frame(i * 2 * STEP, false, true);
        }
        assert_eq!(pacer.stats().skipped_base_rate, 0);
        assert!(pacer.stats().generated > 390);
    }

    /// A copy by `clone_from` is the same pacer as a clone, and reuses the target's interval
    /// history: the frame-generation present path copies the pacer every tick (rev-56
    /// shader-present finding 1).
    #[test]
    fn clone_from_copies_everything_into_the_same_buffer() {
        let mut pacer = FramePacer::new(PacingConfig {
            min_base_fps: 30.0,
            ..PacingConfig::default()
        });
        for frame in 0..40 {
            pacer.on_real_frame(frame * 10_000_000, frame == 20, true);
        }
        let mut copy = FramePacer::default();
        copy.clone_from(&pacer);
        assert_eq!(format!("{copy:?}"), format!("{:?}", pacer.clone()));
        let buffer = copy.intervals.as_slices().0.as_ptr();
        let capacity = copy.intervals.capacity();
        for frame in 40..80 {
            pacer.on_real_frame(frame * 10_000_000, false, true);
            copy.clone_from(&pacer);
            assert_eq!(format!("{copy:?}"), format!("{pacer:?}"));
        }
        assert_eq!(copy.intervals.capacity(), capacity);
        assert_eq!(
            copy.intervals.as_slices().0.as_ptr(),
            buffer,
            "the history was reallocated"
        );
    }
}
