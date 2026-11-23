// SPDX-License-Identifier: MPL-2.0
//! The playback handle of one live sound (`docs/specs/16-audio.md` § 2).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kira::{
    sound::static_sound::StaticSoundHandle,
    sound::{FromFileError, PlaybackState, streaming::StreamingSoundHandle},
};

use crate::bus::tween;

/// Which mixer setter a test is counting. Release builds record nothing.
enum CommandKind {
    Volume,
    Pan,
    Pitch,
}

fn note_command(kind: CommandKind) {
    #[cfg(test)]
    command_counts::note(kind);
    #[cfg(not(test))]
    {
        let _ = kind;
    }
}

/// How many times the three mixer setters have run since `reset`, on the calling thread.
///
/// Thread-local, so test-scoped: libtest runs each test on its own thread and the setters run
/// on the caller's thread, so a parallel test's setter calls never reach another test's counts
/// (process-global counters made `a_still_emitter_does_not_retarget_every_tick` flaky).
///
/// Integration builds of the library (the ones `tests/` link) do not see this module: the
/// counter exists only while the crate itself is compiled for its unit tests.
#[cfg(test)]
pub(crate) mod command_counts {
    use std::cell::Cell;

    use super::CommandKind;

    thread_local! {
        /// `(volume, pan, pitch)` setter calls made on this thread since the last `reset`.
        static COUNTS: Cell<(u64, u64, u64)> = const { Cell::new((0, 0, 0)) };
    }

    /// Counts one setter call on this thread.
    pub(super) fn note(kind: CommandKind) {
        COUNTS.with(|counts| {
            let (volume, pan, pitch) = counts.get();
            counts.set(match kind {
                CommandKind::Volume => (volume + 1, pan, pitch),
                CommandKind::Pan => (volume, pan + 1, pitch),
                CommandKind::Pitch => (volume, pan, pitch + 1),
            });
        });
    }

    /// Zeroes this thread's three counters.
    pub fn reset() {
        COUNTS.with(|counts| counts.set((0, 0, 0)));
    }

    /// `(volume, pan, pitch)` calls on this thread since the last [`reset`].
    pub fn snapshot() -> (u64, u64, u64) {
        COUNTS.with(Cell::get)
    }
}

/// The shortest fade a stop, pause or resume gets once a sound has been heard (ADR-544). kira
/// ends a zero-length fade at the next block boundary, cutting the waveform wherever it is: an
/// audible click. 10 ms is below what reads as a fade and long enough that the last block before
/// the cut is ≥ 45 dB down. A sound that has not played a sample yet still stops at once (a
/// voice stolen in the frame it started never sounds).
pub const DECLICK_FADE: Duration = Duration::from_millis(10);

/// Controls one playing sound: stop/pause/resume with fades, volume/pitch/pan tweens, seek and
/// state. Wraps the kira handle of whichever source kind is playing.
pub struct SoundInstance {
    inner: InstanceKind,
    /// The playback position when the handle was created.
    start_position: f64,
    /// The output's block clock and its value when the voice was registered: a later value
    /// means the mixer has run since. `None` (not registered as a voice) counts as heard.
    render_clock: Option<(Arc<AtomicU64>, u64)>,
}

enum InstanceKind {
    Static(StaticSoundHandle),
    Streaming(Box<StreamingSoundHandle<FromFileError>>),
}

impl SoundInstance {
    /// Starts a static sound from its kira handle.
    pub(crate) fn from_static(handle: StaticSoundHandle) -> Self {
        let start_position = handle.position();
        Self {
            inner: InstanceKind::Static(handle),
            start_position,
            render_clock: None,
        }
    }

    /// Starts a streaming sound from its kira handle.
    pub(crate) fn from_streaming(handle: StreamingSoundHandle<FromFileError>) -> Self {
        let start_position = handle.position();
        Self {
            inner: InstanceKind::Streaming(Box::new(handle)),
            start_position,
            render_clock: None,
        }
    }

    /// Starts watching the output's block clock (the voice manager does this when it registers
    /// the voice, before any block can have rendered it).
    pub(crate) fn watch_render_clock(&mut self, clock: &Arc<AtomicU64>) {
        let now = clock.load(Ordering::Acquire);
        self.render_clock = Some((Arc::clone(clock), now));
    }

    /// False only when the mixer cannot have rendered a sample of the sound yet: its position
    /// has not moved and no block ran since it was registered. kira publishes the position at
    /// the start of the following block, so the position alone lags by one block.
    pub(crate) fn may_have_played(&self) -> bool {
        if self.position() != self.start_position {
            return true;
        }
        self.render_clock
            .as_ref()
            .is_none_or(|(clock, at)| clock.load(Ordering::Acquire) > *at)
    }

    /// `fade`, raised to [`DECLICK_FADE`] for a sound that may have been heard.
    fn declicked(&self, fade: Duration) -> Duration {
        if fade < DECLICK_FADE && self.may_have_played() {
            DECLICK_FADE
        } else {
            fade
        }
    }

    /// Fades the sound to silence over `fade` (at least [`DECLICK_FADE`] once it may have been
    /// heard) and stops it. A stopped sound cannot be restarted.
    pub fn stop(&mut self, fade: Duration) {
        let fade = self.declicked(fade);
        match &mut self.inner {
            InstanceKind::Static(h) => h.stop(tween(fade)),
            InstanceKind::Streaming(h) => h.stop(tween(fade)),
        }
    }

    /// [`SoundInstance::stop`] with the fade-out half of an equal-power crossfade (music track
    /// changes), so the outgoing sound and one started with the matching fade-in hold the level.
    pub(crate) fn stop_crossfading(&mut self, fade: Duration) {
        let fade = crate::sound::equal_power_fade_out(self.declicked(fade));
        match &mut self.inner {
            InstanceKind::Static(h) => h.stop(fade),
            InstanceKind::Streaming(h) => h.stop(fade),
        }
    }

    /// Pauses, fading to silence over `fade` (at least [`DECLICK_FADE`] once it may have been
    /// heard).
    pub fn pause(&mut self, fade: Duration) {
        let fade = self.declicked(fade);
        match &mut self.inner {
            InstanceKind::Static(h) => h.pause(tween(fade)),
            InstanceKind::Streaming(h) => h.pause(tween(fade)),
        }
    }

    /// Resumes, fading in over `fade` (at least [`DECLICK_FADE`]).
    pub fn resume(&mut self, fade: Duration) {
        let fade = fade.max(DECLICK_FADE);
        match &mut self.inner {
            InstanceKind::Static(h) => h.resume(tween(fade)),
            InstanceKind::Streaming(h) => h.resume(tween(fade)),
        }
    }

    /// Retimes the volume (dB, pre-bus).
    pub fn set_volume_db(&mut self, db: f32, fade: Duration) {
        note_command(CommandKind::Volume);
        let db = crate::sound::safe_volume_db(db);
        match &mut self.inner {
            InstanceKind::Static(h) => h.set_volume(kira::Decibels(db), tween(fade)),
            InstanceKind::Streaming(h) => h.set_volume(kira::Decibels(db), tween(fade)),
        }
    }

    /// Retimes the playback rate (1 = original speed and pitch).
    pub fn set_pitch(&mut self, ratio: f32, fade: Duration) {
        note_command(CommandKind::Pitch);
        let rate = kira::PlaybackRate(crate::sound::safe_playback_rate(ratio));
        match &mut self.inner {
            InstanceKind::Static(h) => h.set_playback_rate(rate, tween(fade)),
            InstanceKind::Streaming(h) => h.set_playback_rate(rate, tween(fade)),
        }
    }

    /// Retimes the stereo panning (−1 hard left .. 1 hard right).
    pub fn set_pan(&mut self, pan: f32, fade: Duration) {
        note_command(CommandKind::Pan);
        let pan = kira::Panning(crate::sound::safe_pan(pan));
        match &mut self.inner {
            InstanceKind::Static(h) => h.set_panning(pan, tween(fade)),
            InstanceKind::Streaming(h) => h.set_panning(pan, tween(fade)),
        }
    }

    /// Jumps to a position in the sound, in seconds.
    pub fn seek(&mut self, secs: f64) {
        match &mut self.inner {
            InstanceKind::Static(h) => h.seek_to(secs),
            InstanceKind::Streaming(h) => h.seek_to(secs),
        }
    }

    /// The playback state as kira reports it.
    pub fn state(&self) -> PlaybackState {
        match &self.inner {
            InstanceKind::Static(h) => h.state(),
            InstanceKind::Streaming(h) => h.state(),
        }
    }

    /// The current playback position in seconds.
    pub fn position(&self) -> f64 {
        match &self.inner {
            InstanceKind::Static(h) => h.position(),
            InstanceKind::Streaming(h) => h.position(),
        }
    }

    /// True when the sound has fully stopped (fade-outs included).
    pub fn is_stopped(&self) -> bool {
        matches!(self.state(), PlaybackState::Stopped)
    }

    /// Pops the last streaming decoder error, if any (streaming only).
    pub fn pop_error(&mut self) -> Option<String> {
        match &mut self.inner {
            InstanceKind::Static(_) => None,
            InstanceKind::Streaming(h) => h.pop_error().map(|e| e.to_string()),
        }
    }
}

impl From<StaticSoundHandle> for SoundInstance {
    fn from(handle: StaticSoundHandle) -> Self {
        Self::from_static(handle)
    }
}

#[cfg(test)]
mod compile_checks {
    use super::*;

    #[test]
    fn sound_instance_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SoundInstance>();
    }

    #[test]
    fn zero_fade_tween_is_immediate() {
        assert_eq!(super::tween(Duration::ZERO).duration, Duration::ZERO);
    }

    /// The setter counts are test-scoped: another test's commands (another thread, as libtest
    /// runs them) never reach this test's counts. This is what made
    /// `a_still_emitter_does_not_retarget_every_tick` flaky under parallel tests.
    #[test]
    fn setter_counts_from_another_thread_stay_out_of_this_tests_counts() {
        command_counts::reset();
        std::thread::spawn(|| {
            for _ in 0..100 {
                note_command(CommandKind::Volume);
                note_command(CommandKind::Pan);
                note_command(CommandKind::Pitch);
            }
        })
        .join()
        .expect("the counting thread ends");
        assert_eq!(
            command_counts::snapshot(),
            (0, 0, 0),
            "another thread's setter calls leaked into this test's counts"
        );
        note_command(CommandKind::Volume);
        note_command(CommandKind::Pitch);
        assert_eq!(
            command_counts::snapshot(),
            (1, 0, 1),
            "this thread's own calls count"
        );
    }
}
