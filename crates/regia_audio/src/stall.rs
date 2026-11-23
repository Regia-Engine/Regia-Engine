// SPDX-License-Identifier: MPL-2.0
//! Output stall watchdog (ADR-8830): notices a device output whose sample clock stopped while the
//! engine believes it is live, and forces one suspend/resume cycle to reopen it.
//!
//! kira's cpal backend restarts its stream itself when the default device changes or dies; when the
//! reopen fails (an unplug race) the restart thread dies and the renderer with it, so the mixer
//! goes silent with no error surfaced (deep audio-mix review, finding 3). The output's
//! [`SampleClock`] is the signal: it advances with every rendered block and freezes with the
//! stream. The watchdog counts *engine frames* in which the clock did not move, and sums their
//! engine time (the unscaled `RealTime` delta, so a game pause does not stop it and a test drives it
//! with a fake clock). Past [`AudioStallWatchdog::stall_frames`] frames *and*
//! [`AudioStallWatchdog::min_stall`] of engine time it suspends and resumes the output once, counts
//! a recovery, logs a `tracing::warn`, and doubles its patience (up to
//! [`AudioStallWatchdog::max_backoff`]) so a device that stays dead is not hammered. The back-off
//! resets once the clock runs again for as long. The time floor keeps the patience from shrinking
//! with the frame rate: 240 frames are a quarter of a second at 1000 fps, and a recovery ends every
//! playing voice (rev-56 audio finding 1).
//!
//! A suspended output (Android `Suspended`, a hidden window, the slow-frame throttle, a host's own
//! suspend), the offline backend and a render-out mixdown are never touched: their clocks stop on
//! purpose. A resume of the watchdog's own that cannot open a device is retried on the same
//! back-off, and never while the surfaces are hidden or the activity is paused.

use std::time::Duration;

use regia_ecs::prelude::*;

use crate::audio::Audio;
use crate::backend::SampleClock;

/// What the watchdog needs of an output: its clock, whether it is watched at all, and the
/// suspend/resume pair. [`Audio`] implements it; tests drive a fake.
pub trait StallTarget {
    /// The output's sample clock (`None` before the first block and while suspended).
    fn sample_clock(&self) -> Option<SampleClock>;
    /// True while the output is suspended (by anyone).
    fn output_suspended(&self) -> bool;
    /// True when the output is a realtime device stream: not the offline renderer and not in a
    /// render-out mixdown. Only a watched output can stall.
    fn watched(&self) -> bool;
    /// Frees the output stream.
    fn suspend_output(&mut self);
    /// Reopens the output stream; `false` when no device could be opened.
    fn resume_output(&mut self) -> bool;
}

impl StallTarget for Audio {
    fn sample_clock(&self) -> Option<SampleClock> {
        Audio::sample_clock(self)
    }

    fn output_suspended(&self) -> bool {
        Audio::output_suspended(self)
    }

    fn watched(&self) -> bool {
        !self.is_headless() && !self.mixdown_active()
    }

    fn suspend_output(&mut self) {
        Audio::suspend_output(self);
    }

    fn resume_output(&mut self) -> bool {
        Audio::resume_output(self)
    }
}

/// What one [`AudioStallWatchdog::observe`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallAction {
    /// Nothing: the clock moved, the output is not watched, or the stall is not long enough yet.
    None,
    /// A recovery cycle ran: the output was suspended and resumed. `reopened` is false when the
    /// resume found no device (it is retried on the back-off).
    Recovered {
        /// Whether the resume reopened the device.
        reopened: bool,
    },
    /// A failed resume of the watchdog's own was retried.
    Retried {
        /// Whether this retry reopened the device.
        reopened: bool,
    },
}

/// The stall watchdog's settings and state, a resource the [`crate::AudioPlugin`] inserts and its
/// `audio_stall_watchdog` system drives once a frame in `PostUpdate`. Insert your own before the
/// plugin to change the settings.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct AudioStallWatchdog {
    /// False switches the watchdog off.
    pub enabled: bool,
    /// Engine frames the sample clock may stand still on a live device output before a recovery
    /// (default [`Self::DEFAULT_STALL_FRAMES`]: about 4 s at 60 fps).
    pub stall_frames: u32,
    /// Engine time (summed `RealTime` delta) the sample clock must also stand still before a
    /// recovery, whatever the frame rate (default [`Self::DEFAULT_MIN_STALL`]). It backs off with
    /// `stall_frames`. Zero counts frames only.
    pub min_stall: Duration,
    /// The largest multiple of `stall_frames` the back-off waits (default 16).
    pub max_backoff: u32,
    last_frames: Option<u64>,
    frozen_for: u32,
    frozen_time: Duration,
    running_for: u32,
    running_time: Duration,
    backoff: u32,
    /// Frames left before retrying a failed resume of the watchdog's own; `None` when none is owed.
    retry_in: Option<u32>,
    /// Engine time left before that retry (it waits for both).
    retry_wait: Duration,
    recoveries: u64,
    failed_resumes: u64,
}

impl Default for AudioStallWatchdog {
    fn default() -> Self {
        Self::new(Self::DEFAULT_STALL_FRAMES)
    }
}

impl AudioStallWatchdog {
    /// The default patience, in engine frames.
    pub const DEFAULT_STALL_FRAMES: u32 = 240;

    /// The default least stall, in engine time: above 80 fps it is the binding one, so a
    /// sub-second callback stall (a Bluetooth switch, a loaded host) never ends the music.
    pub const DEFAULT_MIN_STALL: Duration = Duration::from_secs(3);

    /// An enabled watchdog that recovers after `stall_frames` frozen frames (at least 1) that
    /// also last [`Self::DEFAULT_MIN_STALL`] of engine time.
    #[must_use]
    pub fn new(stall_frames: u32) -> Self {
        Self {
            enabled: true,
            stall_frames: stall_frames.max(1),
            min_stall: Self::DEFAULT_MIN_STALL,
            max_backoff: 16,
            last_frames: None,
            frozen_for: 0,
            frozen_time: Duration::ZERO,
            running_for: 0,
            running_time: Duration::ZERO,
            backoff: 1,
            retry_in: None,
            retry_wait: Duration::ZERO,
            recoveries: 0,
            failed_resumes: 0,
        }
    }

    /// Recovery cycles run so far (each a suspend followed by a resume).
    pub fn recoveries(&self) -> u64 {
        self.recoveries
    }

    /// Resumes of the watchdog's own (cycles and retries) that found no device.
    pub fn failed_resumes(&self) -> u64 {
        self.failed_resumes
    }

    /// Consecutive watched frames in which the clock has not moved.
    pub fn frozen_frames(&self) -> u32 {
        self.frozen_for
    }

    /// The engine time of those frames.
    pub fn frozen_time(&self) -> Duration {
        self.frozen_time
    }

    /// The current back-off multiple of `stall_frames` (1 = no back-off).
    pub fn backoff(&self) -> u32 {
        self.backoff
    }

    /// True while a resume of the watchdog's own failed and is waiting to be retried.
    pub fn retry_pending(&self) -> bool {
        self.retry_in.is_some()
    }

    fn patience(&self) -> u32 {
        self.stall_frames.saturating_mul(self.backoff)
    }

    fn patience_time(&self) -> Duration {
        self.min_stall.saturating_mul(self.backoff)
    }

    fn back_off(&mut self) {
        self.backoff = self.backoff.saturating_mul(2).min(self.max_backoff.max(1));
    }

    fn forget_clock(&mut self) {
        self.last_frames = None;
        self.frozen_for = 0;
        self.frozen_time = Duration::ZERO;
        self.running_for = 0;
        self.running_time = Duration::ZERO;
    }

    /// One engine frame that lasted `delta` (the unscaled `RealTime` delta; a test passes its fake
    /// clock's step): watches `target`'s clock and acts as the module docs describe. `hold` is true
    /// while the output must stay quiet anyway (every surface hidden, activity paused): no retry of
    /// a failed resume then.
    pub fn observe(&mut self, target: &mut impl StallTarget, hold: bool, delta: Duration) -> StallAction {
        if !self.enabled || !target.watched() {
            self.retry_in = None;
            self.forget_clock();
            return StallAction::None;
        }
        if let Some(left) = self.retry_in {
            // Someone else resumed the output: the debt is paid.
            if !target.output_suspended() {
                self.retry_in = None;
                self.forget_clock();
                return StallAction::None;
            }
            self.retry_wait = self.retry_wait.saturating_sub(delta);
            if left > 1 || hold || !self.retry_wait.is_zero() {
                self.retry_in = Some(left.saturating_sub(1).max(1));
                return StallAction::None;
            }
            let reopened = target.resume_output();
            self.retry_in = if reopened {
                None
            } else {
                self.failed_resumes += 1;
                self.back_off();
                self.retry_wait = self.patience_time();
                Some(self.patience())
            };
            tracing::warn!(
                target: "regia_audio",
                reopened,
                failed_resumes = self.failed_resumes,
                "audio output stall: retried reopening the output device"
            );
            return StallAction::Retried { reopened };
        }
        // A suspended output stopped its clock on purpose: never poke it.
        if target.output_suspended() {
            self.forget_clock();
            return StallAction::None;
        }
        let now = target.sample_clock().map(|c| c.frames);
        if self.last_frames.is_some() && now != self.last_frames {
            self.frozen_for = 0;
            self.frozen_time = Duration::ZERO;
            self.running_for = self.running_for.saturating_add(1);
            self.running_time = self.running_time.saturating_add(delta);
            if self.running_for >= self.stall_frames && self.running_time >= self.min_stall {
                self.backoff = 1;
            }
            self.last_frames = now;
            return StallAction::None;
        }
        if self.last_frames.is_none() && now.is_some() {
            // The first reading (or the first after a reset) starts the watch.
            self.last_frames = now;
            self.frozen_for = 0;
            self.frozen_time = Duration::ZERO;
            return StallAction::None;
        }
        self.running_for = 0;
        self.running_time = Duration::ZERO;
        self.frozen_for = self.frozen_for.saturating_add(1);
        self.frozen_time = self.frozen_time.saturating_add(delta);
        if self.frozen_for < self.patience() || self.frozen_time < self.patience_time() {
            return StallAction::None;
        }
        let frozen = self.frozen_for;
        let frozen_ms = self.frozen_time.as_millis();
        target.suspend_output();
        let reopened = target.resume_output();
        self.recoveries += 1;
        if !reopened {
            self.failed_resumes += 1;
        }
        self.back_off();
        self.retry_in = (!reopened).then(|| self.patience());
        self.retry_wait = if reopened {
            Duration::ZERO
        } else {
            self.patience_time()
        };
        // The rebuilt output's clock carries on from the frozen value: watch it afresh.
        self.forget_clock();
        tracing::warn!(
            target: "regia_audio",
            frozen_frames = frozen,
            frozen_ms,
            reopened,
            recoveries = self.recoveries,
            next_patience_frames = self.patience(),
            next_patience_ms = self.patience_time().as_millis(),
            "audio output stall: the sample clock stood still for {frozen} frames ({frozen_ms} ms) \
             on a live output; suspended and resumed it"
        );
        StallAction::Recovered { reopened }
    }
}

/// `PostUpdate`/`AudioSync` system (after the output gate): runs [`AudioStallWatchdog::observe`]
/// on the [`Audio`] resource once a frame, with the frame's unscaled `RealTime` delta. Inert
/// without either resource.
pub fn audio_stall_watchdog(
    hidden: Option<Res<regia_platform::SurfacesHidden>>,
    activity: Option<Res<regia_platform::ActivityState>>,
    real: Option<Res<regia_app::RealTime>>,
    watchdog: Option<ResMut<AudioStallWatchdog>>,
    audio: Option<ResMut<Audio>>,
) {
    let _span = tracing::trace_span!("audio_stall_watchdog").entered();
    let (Some(mut watchdog), Some(mut audio)) = (watchdog, audio) else {
        return;
    };
    let hold = hidden.is_some_and(|h| h.0) || activity.is_some_and(|a| a.paused);
    // Without a clock no time passes: only a watchdog that counts frames alone can act.
    let delta = real.map_or(Duration::ZERO, |real| real.delta());
    let _ = watchdog.observe(&mut *audio, hold, delta);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device output under test control: a clock that advances only when told to, and counters
    /// of every suspend and resume the watchdog asks for.
    #[derive(Default)]
    struct FakeOutput {
        frames: u64,
        suspended: bool,
        offline: bool,
        reopen_fails: bool,
        suspends: u32,
        resumes: u32,
    }

    impl FakeOutput {
        fn live() -> Self {
            Self {
                frames: 512,
                ..Self::default()
            }
        }
    }

    impl StallTarget for FakeOutput {
        fn sample_clock(&self) -> Option<SampleClock> {
            (!self.suspended && self.frames > 0).then_some(SampleClock {
                frames: self.frames,
                sample_rate: 48_000,
            })
        }

        fn output_suspended(&self) -> bool {
            self.suspended
        }

        fn watched(&self) -> bool {
            !self.offline
        }

        fn suspend_output(&mut self) {
            if !self.suspended {
                self.suspended = true;
                self.suspends += 1;
            }
        }

        fn resume_output(&mut self) -> bool {
            self.resumes += 1;
            if self.reopen_fails {
                return false;
            }
            self.suspended = false;
            true
        }
    }

    /// One frame at 60 fps on the fake clock.
    const FRAME_60: Duration = Duration::from_nanos(16_666_667);
    /// One frame at 1000 fps.
    const FRAME_1000: Duration = Duration::from_millis(1);

    /// A watchdog that counts frames only (no time floor): the frame rules on their own.
    fn frames_only(stall_frames: u32) -> AudioStallWatchdog {
        AudioStallWatchdog {
            min_stall: Duration::ZERO,
            ..AudioStallWatchdog::new(stall_frames)
        }
    }

    /// `frames` frames of `delta` each, the clock moving by `advance` a frame; how many acted.
    fn run_at(
        dog: &mut AudioStallWatchdog,
        out: &mut FakeOutput,
        frames: u32,
        advance: u64,
        delta: Duration,
    ) -> u32 {
        let mut acted = 0;
        for _ in 0..frames {
            out.frames += advance;
            if dog.observe(out, false, delta) != StallAction::None {
                acted += 1;
            }
        }
        acted
    }

    /// [`run_at`] at 60 fps.
    fn run(dog: &mut AudioStallWatchdog, out: &mut FakeOutput, frames: u32, advance: u64) -> u32 {
        run_at(dog, out, frames, advance, FRAME_60)
    }

    #[test]
    fn a_frozen_clock_triggers_exactly_one_recovery_cycle() {
        let mut dog = frames_only(10);
        let mut out = FakeOutput::live();
        assert_eq!(
            run(&mut dog, &mut out, 10, 0),
            0,
            "not yet: the first reading plus 9 frozen"
        );
        assert_eq!(
            run(&mut dog, &mut out, 1, 0),
            1,
            "the tenth frozen frame recovers"
        );
        assert_eq!((out.suspends, out.resumes, dog.recoveries()), (1, 1, 1));
        assert!(!out.suspended, "the output is live again");
        // Still frozen: the back-off doubles the patience, so nothing for the next 20 frames.
        assert_eq!(run(&mut dog, &mut out, 20, 0), 0);
        assert_eq!((out.suspends, out.resumes, dog.recoveries()), (1, 1, 1));
        assert_eq!(dog.backoff(), 2);
    }

    #[test]
    fn an_advancing_clock_does_nothing() {
        let mut dog = frames_only(10);
        let mut out = FakeOutput::live();
        assert_eq!(run(&mut dog, &mut out, 1_000, 512), 0);
        assert_eq!((out.suspends, out.resumes, dog.recoveries()), (0, 0, 0));
        // A clock that moves only every few frames (a large device buffer) is not a stall either.
        for _ in 0..200 {
            assert_eq!(run(&mut dog, &mut out, 4, 0), 0);
            out.frames += 2_048;
        }
        assert_eq!(dog.recoveries(), 0);
    }

    #[test]
    fn a_suspended_output_is_never_poked() {
        let mut dog = frames_only(10);
        let mut out = FakeOutput::live();
        out.suspend_output();
        assert_eq!(run(&mut dog, &mut out, 1_000, 0), 0);
        assert_eq!((out.suspends, out.resumes, dog.recoveries()), (1, 0, 0));
        // Resumed by its owner: the watch starts afresh, no credit from the suspended frames.
        assert!(out.resume_output());
        assert_eq!(run(&mut dog, &mut out, 10, 0), 0);
        assert_eq!(run(&mut dog, &mut out, 1, 0), 1);
    }

    #[test]
    fn an_offline_output_and_a_disabled_watchdog_are_never_poked() {
        let mut dog = frames_only(10);
        let mut out = FakeOutput {
            offline: true,
            ..FakeOutput::live()
        };
        assert_eq!(run(&mut dog, &mut out, 1_000, 0), 0);
        let mut dog = AudioStallWatchdog {
            enabled: false,
            ..frames_only(10)
        };
        let mut out = FakeOutput::live();
        assert_eq!(run(&mut dog, &mut out, 1_000, 0), 0);
        assert_eq!((out.suspends, out.resumes), (0, 0));
    }

    #[test]
    fn a_failed_reopen_is_retried_on_the_back_off_and_not_while_held() {
        let mut dog = frames_only(10);
        let mut out = FakeOutput {
            reopen_fails: true,
            ..FakeOutput::live()
        };
        assert_eq!(run(&mut dog, &mut out, 11, 0), 1);
        assert_eq!((dog.recoveries(), dog.failed_resumes()), (1, 1));
        assert!(out.suspended && dog.retry_pending());
        // Held (hidden window): the retry waits however long it takes.
        for _ in 0..100 {
            assert_eq!(dog.observe(&mut out, true, FRAME_60), StallAction::None);
        }
        assert_eq!(out.resumes, 1);
        // Released: the retry comes due (it already waited) and fails again, then backs off to 40.
        assert_eq!(
            dog.observe(&mut out, false, FRAME_60),
            StallAction::Retried { reopened: false }
        );
        assert_eq!((out.resumes, dog.backoff()), (2, 4));
        assert_eq!(run(&mut dog, &mut out, 39, 0), 0);
        out.reopen_fails = false;
        assert_eq!(
            dog.observe(&mut out, false, FRAME_60),
            StallAction::Retried { reopened: true }
        );
        assert!(!out.suspended && !dog.retry_pending());
        assert_eq!(dog.recoveries(), 1, "a retry is not a new cycle");
    }

    #[test]
    fn a_running_clock_resets_the_back_off() {
        let mut dog = frames_only(10);
        let mut out = FakeOutput::live();
        assert_eq!(run(&mut dog, &mut out, 11, 0), 1);
        assert_eq!(dog.backoff(), 2);
        // The first frame after a cycle only takes a reading; ten running frames follow it.
        assert_eq!(run(&mut dog, &mut out, 11, 512), 0);
        assert_eq!(dog.backoff(), 1, "ten running frames restore the patience");
        assert_eq!(
            run(&mut dog, &mut out, 10, 0),
            1,
            "a new stall recovers after ten frames"
        );
        assert_eq!(dog.recoveries(), 2);
    }

    /// The plugin inserts the watchdog and runs it every frame; a headless app's offline output
    /// (whose clock only moves when a test renders) is never suspended by it.
    #[test]
    fn the_plugin_runs_the_watchdog_and_leaves_an_offline_output_alone() {
        let mut app = regia_app::App::new();
        app.insert_resource(crate::AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.insert_resource(AudioStallWatchdog::new(2));
        app.add_plugins(crate::AudioPlugin);
        for _ in 0..50 {
            app.update();
        }
        let audio = app.world().resource::<Audio>();
        assert_eq!((audio.suspend_count(), audio.resume_count()), (0, 0));
        let dog = app.world().resource::<AudioStallWatchdog>();
        assert_eq!(
            (dog.stall_frames, dog.recoveries(), dog.frozen_frames()),
            (2, 0, 0)
        );
    }

    /// rev-56 audio finding 1: the patience is counted in frames, so it shrinks with the frame
    /// rate. An uncapped 1000 fps menu must not take a quarter-second callback stall (a Bluetooth
    /// switch, a loaded host) for a dead output: a recovery ends every playing voice.
    #[test]
    fn a_quarter_second_stall_at_1000_fps_is_left_alone() {
        let mut dog = AudioStallWatchdog::default();
        let mut out = FakeOutput::live();
        // One second of a running output at 1000 fps (48 samples a frame at 48 kHz).
        assert_eq!(run_at(&mut dog, &mut out, 1_000, 48, FRAME_1000), 0);
        // The callback stalls for 250 frames, a quarter of a second.
        assert_eq!(
            run_at(&mut dog, &mut out, 250, 0, FRAME_1000),
            0,
            "a 0.25 s stall at 1000 fps suspended and resumed the output"
        );
        assert_eq!((out.suspends, out.resumes, dog.recoveries()), (0, 0, 0));
        // The clock runs again: the stall is forgotten.
        assert_eq!(run_at(&mut dog, &mut out, 10, 48, FRAME_1000), 0);
        assert_eq!(dog.frozen_time(), Duration::ZERO);
    }

    /// At 1000 fps a dead output is still recovered, once the stall lasts the time floor:
    /// 3000 frames, not 240.
    #[test]
    fn at_1000_fps_the_time_floor_sets_the_patience() {
        let mut dog = AudioStallWatchdog::default();
        let mut out = FakeOutput::live();
        assert_eq!(run_at(&mut dog, &mut out, 1, 48, FRAME_1000), 0);
        assert_eq!(run_at(&mut dog, &mut out, 2_999, 0, FRAME_1000), 0);
        assert_eq!(dog.frozen_frames(), 2_999);
        assert_eq!(
            run_at(&mut dog, &mut out, 1, 0, FRAME_1000),
            1,
            "three seconds of a frozen clock recover"
        );
        assert_eq!((out.suspends, out.resumes, dog.recoveries()), (1, 1, 1));
        // Still dead: the back-off doubles the time too, 6 s (not 480 frames).
        assert_eq!(dog.backoff(), 2);
        assert_eq!(run_at(&mut dog, &mut out, 1, 0, FRAME_1000), 0, "a fresh reading");
        assert_eq!(run_at(&mut dog, &mut out, 5_999, 0, FRAME_1000), 0);
        assert_eq!(run_at(&mut dog, &mut out, 1, 0, FRAME_1000), 1);
        assert_eq!(dog.recoveries(), 2);
    }

    /// At 60 fps the frame count binds, as before the time floor: 240 frames, 4 s.
    #[test]
    fn at_60_fps_the_frame_count_sets_the_patience() {
        let mut dog = AudioStallWatchdog::default();
        let mut out = FakeOutput::live();
        assert_eq!(run(&mut dog, &mut out, 600, 800), 0);
        // 239 frozen frames are 3.98 s: past the time floor, short of the frames.
        assert_eq!(run(&mut dog, &mut out, 239, 0), 0);
        assert!(dog.frozen_time() > AudioStallWatchdog::DEFAULT_MIN_STALL);
        assert_eq!(
            run(&mut dog, &mut out, 1, 0),
            1,
            "the 240th frozen frame recovers"
        );
        assert_eq!(dog.recoveries(), 1);
    }

    /// At 1000 fps a failed reopen is retried on the back-off's time, not its frame count, and the
    /// back-off resets only after the clock ran for the time floor.
    #[test]
    fn at_1000_fps_retries_and_the_back_off_reset_wait_for_time() {
        let mut dog = AudioStallWatchdog::default();
        let mut out = FakeOutput {
            reopen_fails: true,
            ..FakeOutput::live()
        };
        assert_eq!(run_at(&mut dog, &mut out, 3_001, 0, FRAME_1000), 1);
        assert!(dog.retry_pending() && out.suspended);
        assert_eq!(out.resumes, 1);
        // 480 frames (the frame back-off) are half a second: no retry yet.
        assert_eq!(run_at(&mut dog, &mut out, 5_999, 0, FRAME_1000), 0);
        assert_eq!(out.resumes, 1, "retried before the 6 s back-off");
        out.reopen_fails = false;
        assert_eq!(
            dog.observe(&mut out, false, FRAME_1000),
            StallAction::Retried { reopened: true }
        );
        assert_eq!(dog.backoff(), 2);
        // 240 running frames are not enough to forget the back-off at 1000 fps.
        assert_eq!(run_at(&mut dog, &mut out, 1 + 240, 48, FRAME_1000), 0);
        assert_eq!(dog.backoff(), 2);
        assert_eq!(run_at(&mut dog, &mut out, 3_000 - 240, 48, FRAME_1000), 0);
        assert_eq!(dog.backoff(), 1, "three running seconds restore the patience");
    }

    #[test]
    fn a_retry_owed_is_dropped_when_the_owner_resumes() {
        let mut dog = frames_only(10);
        let mut out = FakeOutput {
            reopen_fails: true,
            ..FakeOutput::live()
        };
        assert_eq!(run(&mut dog, &mut out, 11, 0), 1);
        assert!(dog.retry_pending());
        out.reopen_fails = false;
        assert!(out.resume_output(), "the host reopened it");
        assert_eq!(dog.observe(&mut out, false, FRAME_60), StallAction::None);
        assert!(!dog.retry_pending());
        assert_eq!(out.resumes, 2, "the watchdog did not resume again");
    }
}
