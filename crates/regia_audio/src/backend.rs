// SPDX-License-Identifier: MPL-2.0
//! Output backends: the default device through kira's cpal backend, or an offline renderer for
//! headless runs and tests (`docs/specs/16-audio.md` § 1).
//!
//! The offline backend implements kira's [`Backend`] trait with a renderer whose output goes to a
//! buffer the caller reads instead of a device, so the whole mixer graph — buses, effects,
//! spatialisation, ducking — is exercised deterministically in CI. On a machine without an output
//! device (or with [`AudioSettings::headless`]) the same backend is used silently.

use std::convert::Infallible;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kira::backend::cpal::CpalBackendSettings;
use kira::{
    AudioManager, AudioManagerSettings, Frame,
    backend::{Backend, Renderer, cpal::CpalBackend},
    clock::{ClockHandle, ClockSpeed},
    effect::{Effect, EffectBuilder},
    info::Info,
    track::MainTrackBuilder,
};

use crate::bus::{BusLayout, Buses, TrackHost};
use crate::error::AudioError;
use crate::limiter::{LimiterBuilder, LimiterShared};
use crate::settings::AudioSettings;

/// A one-shot accepted by the mixer: the metadata recorded per play, used by tests and headless
/// runs to verify what would have been heard.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayedOneShot {
    /// Number of samples in the played clip.
    pub clip_len: usize,
    /// Sample rate of the played clip, in Hz.
    pub sample_rate: u32,
    /// Position the one-shot was played at, or `None` for a non-positional play.
    pub position: Option<regia_core::math::Vec3>,
}

/// The output's sample clock: the frames it has rendered since it opened (spec 56 § 2, ADR-1882).
/// Unlike a wall clock it stops with the stream and advances exactly with what is heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleClock {
    /// Frames rendered. An output rebuilt after a suspend keeps counting from here.
    pub frames: u64,
    /// The output's sample rate in Hz.
    pub sample_rate: u32,
}

impl SampleClock {
    /// The clock in microseconds: `frames * 1_000_000 / sample_rate`, integer, rounded down.
    #[must_use]
    pub fn micros(self) -> u64 {
        let micros = u128::from(self.frames) * 1_000_000 / u128::from(self.sample_rate.max(1));
        u64::try_from(micros).unwrap_or(u64::MAX)
    }
}

/// The counter behind [`SampleClock`], shared with the effect on the audio thread.
#[derive(Debug, Default)]
pub(crate) struct OutputClock {
    frames: AtomicU64,
    sample_rate: AtomicU32,
}

impl OutputClock {
    fn read(&self) -> Option<SampleClock> {
        let frames = self.frames.load(Ordering::Acquire);
        let sample_rate = self.sample_rate.load(Ordering::Relaxed);
        (frames > 0 && sample_rate > 0).then_some(SampleClock { frames, sample_rate })
    }
}

/// Counts the frames of every block the main track renders. Allocation-free.
struct OutputClockEffect(Arc<OutputClock>);

impl Effect for OutputClockEffect {
    fn process(&mut self, input: &mut [Frame], dt: f64, _info: &Info) {
        if dt > 0.0 {
            self.0
                .sample_rate
                .store((1.0 / dt).round() as u32, Ordering::Relaxed);
        }
        self.0.frames.fetch_add(input.len() as u64, Ordering::Release);
    }
}

/// Builds the counting effect for a main track.
struct OutputClockBuilder(Arc<OutputClock>);

impl EffectBuilder for OutputClockBuilder {
    type Handle = ();

    fn build(self) -> (Box<dyn Effect>, Self::Handle) {
        (Box::new(OutputClockEffect(self.0)), ())
    }
}

/// Settings of the offline backend: just the sample rate the mixer runs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfflineSettings {
    /// Sample rate in Hz the renderer advances at.
    pub sample_rate: u32,
}

impl Default for OfflineSettings {
    fn default() -> Self {
        Self { sample_rate: 48_000 }
    }
}

/// A kira backend that renders into a caller-owned buffer instead of an audio device. Time only
/// advances when [`OfflineBackend::render`] is called.
pub struct OfflineBackend {
    renderer: Option<Renderer>,
}

impl Backend for OfflineBackend {
    type Settings = OfflineSettings;
    type Error = Infallible;

    fn setup(settings: Self::Settings, _internal_buffer_size: usize) -> Result<(Self, u32), Self::Error> {
        Ok((Self { renderer: None }, settings.sample_rate))
    }

    fn start(&mut self, renderer: Renderer) -> Result<(), Self::Error> {
        self.renderer = Some(renderer);
        Ok(())
    }
}

impl OfflineBackend {
    /// Renders `frames` stereo frames (interleaved left, right), advancing the mixer by
    /// `frames / sample_rate` seconds. An unstarted backend renders silence.
    pub fn render(&mut self, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        let Some(renderer) = self.renderer.as_mut() else {
            return out;
        };
        renderer.on_start_processing();
        renderer.process(&mut out, 2);
        out
    }
}

/// Where sound goes (a real output device or the offline renderer), and the master limiter
/// every output's main track runs (ADR-544). The limiter state outlives an output rebuild.
pub(crate) struct OutputBackend {
    output: Output,
    limiter: Arc<LimiterShared>,
    /// Frames the output (not a mixdown) has rendered; outlives an output rebuild.
    clock: Arc<OutputClock>,
    /// The output a render-out mixdown set aside while its own offline mixer runs
    /// (`Audio::begin_mixdown`); `None` outside a mixdown.
    stashed: Option<Output>,
    /// The mixdown mixer's own limiter state (settings copied in): the set-aside output keeps
    /// ticking on `limiter`, and neither must count into the other's meter or block clock
    /// (concurrency audit 11-03 L1). `None` outside a mixdown.
    mixdown_limiter: Option<Arc<LimiterShared>>,
}

/// The output itself.
enum Output {
    /// The default output device through cpal. The manager sits behind a mutex (boxed to keep the
    /// enum small) because cpal's streams are `Send` but not `Sync` on every platform and
    /// resources require `Sync`; the world's `&mut` access already excludes other users, so the
    /// mutex is never contended. `None` while suspended (the stream is freed, spec § 4).
    Device(Box<Mutex<Option<AudioManager<CpalBackend>>>>),
    /// The offline renderer, for headless runs and tests (boxed: the renderer is far larger
    /// than the device handle).
    Offline(Box<Mutex<AudioManager<OfflineBackend>>>),
}

impl OutputBackend {
    /// Opens the backend for `settings`: the offline renderer when `headless`, otherwise the
    /// default output device, falling back to the offline renderer when the device cannot be
    /// opened (no device, no audio server). Never panics and never fails.
    pub(crate) fn open(settings: &AudioSettings) -> Self {
        let limiter = Arc::new(LimiterShared::default());
        let clock = Arc::new(OutputClock::default());
        let output = if settings.headless {
            tracing::debug!(target: "regia_audio", "headless: using the offline audio backend");
            Output::offline(settings.sample_rate, &limiter, &clock)
        } else {
            match Output::device(&limiter, &clock) {
                Some(manager) => {
                    tracing::info!(target: "regia_audio", "audio output device opened through kira");
                    Output::Device(Box::new(Mutex::new(Some(manager))))
                }
                None => {
                    tracing::warn!(
                        target: "regia_audio",
                        "no audio output device available; using the offline backend"
                    );
                    Output::offline(settings.sample_rate, &limiter, &clock)
                }
            }
        };
        Self {
            output,
            limiter,
            clock,
            stashed: None,
            mixdown_limiter: None,
        }
    }

    /// Sets the current output aside and runs a fresh offline mixer at `sample_rate` with room
    /// for `send_tracks` send tracks (a render-out mixdown). The old output keeps its stream but
    /// is fed nothing until [`Self::end_mixdown`]. `false` when a mixdown already runs.
    pub(crate) fn begin_mixdown(&mut self, sample_rate: u32, send_tracks: usize) -> bool {
        if self.stashed.is_some() {
            return false;
        }
        // A mixdown runs faster than real time and at its own rate: it does not count as output,
        // neither on the output clock nor in the output's limiter meter.
        let limiter = Arc::new(LimiterShared::default());
        limiter.set(self.limiter.get());
        let mixdown = Output::offline_with(sample_rate, send_tracks, &limiter, None);
        self.mixdown_limiter = Some(limiter);
        self.stashed = Some(std::mem::replace(&mut self.output, mixdown));
        true
    }

    /// Drops the mixdown's mixer and puts the output set aside by [`Self::begin_mixdown`] back.
    /// `false` when no mixdown runs.
    pub(crate) fn end_mixdown(&mut self) -> bool {
        match self.stashed.take() {
            Some(output) => {
                self.output = output;
                self.mixdown_limiter = None;
                true
            }
            None => false,
        }
    }

    /// True while a render-out mixdown runs on its own offline mixer.
    pub(crate) fn in_mixdown(&self) -> bool {
        self.stashed.is_some()
    }

    /// The master limiter's shared state (settings and meter) of the mixer that renders now: the
    /// mixdown's during a render-out mixdown, else the output's.
    pub(crate) fn limiter(&self) -> &LimiterShared {
        self.mixdown_limiter.as_deref().unwrap_or(&self.limiter)
    }

    /// Applies new master-limiter settings to the output and, during a mixdown, to the mixdown
    /// mixer too, so the setting survives the mixdown's end.
    pub(crate) fn set_limiter(&self, limiter: Option<crate::limiter::MasterLimiter>) {
        self.limiter.set(limiter);
        if let Some(mixdown) = &self.mixdown_limiter {
            mixdown.set(limiter);
        }
    }

    /// The output's sample clock. `None` before its first rendered block, while suspended and
    /// during a render-out mixdown: nothing is being heard on that clock then.
    pub(crate) fn sample_clock(&self) -> Option<SampleClock> {
        if self.in_mixdown() {
            return None;
        }
        if let Output::Device(manager) = &self.output
            && lock(manager).is_none()
        {
            return None;
        }
        self.clock.read()
    }

    /// True when this is the offline renderer.
    pub(crate) fn is_offline(&self) -> bool {
        matches!(self.output, Output::Offline(_))
    }

    /// Frees the output stream (Android suspend, spec § 4). The offline backend keeps running so
    /// headless tests can continue; on a device all playing sounds stop with the teardown — kira
    /// does not expose the cpal stream, so the manager is dropped wholesale. During a render-out
    /// mixdown the stashed output is torn down too: the take does not need it, and a `Resumed`
    /// mid-take must not find it still streaming.
    pub(crate) fn suspend(&mut self) {
        if let Output::Device(manager) = &mut self.output {
            *lock(manager) = None;
        }
        if let Some(Output::Device(manager)) = &mut self.stashed {
            *lock(manager) = None;
        }
    }

    /// Re-opens the output stream after a suspend. The facade rebuilds the bus tree and then
    /// restores persistent sounds (`docs/specs/16-audio.md` § 4).
    pub(crate) fn resume(&mut self) -> bool {
        match &mut self.output {
            Output::Device(manager) => {
                let mut guard = lock(manager);
                if guard.is_none() {
                    *guard = Output::device(&self.limiter, &self.clock);
                }
                guard.is_some()
            }
            Output::Offline(manager) => {
                // No device callback services this mixer. Retire previously dropped tracks
                // before rebuilding, even when a host cycles output without rendering audio.
                // A zero-frame block processes commands without advancing the sample clock.
                lock(manager).backend_mut().render(0);
                true
            }
        }
    }

    /// Builds the bus tree on the active manager. `Err(UnknownBus)` while suspended.
    pub(crate) fn build_buses(
        &mut self,
        layout: &BusLayout,
        conv: &crate::bus::ConvolutionContext,
    ) -> Result<Buses, AudioError> {
        self.build_buses_tapped(layout, &[], conv)
    }

    /// [`Self::build_buses`] with render-out stem taps (`Buses::build_tapped`, ADR-1520).
    pub(crate) fn build_buses_tapped(
        &mut self,
        layout: &BusLayout,
        taps: &[(String, crate::audio::StemTap)],
        conv: &crate::bus::ConvolutionContext,
    ) -> Result<Buses, AudioError> {
        match &mut self.output {
            Output::Device(manager) => {
                let mut guard = lock(manager);
                match guard.as_mut() {
                    Some(manager) => {
                        Buses::build_tapped(layout, manager, taps, conv).map_err(AudioError::PlayFailed)
                    }
                    None => Err(AudioError::UnknownBus("suspended".into())),
                }
            }
            Output::Offline(manager) => {
                let mut guard = lock(manager);
                Buses::build_tapped(layout, &mut *guard, taps, conv).map_err(AudioError::PlayFailed)
            }
        }
    }

    /// Adds a ticking clock to the active manager (music bar timing). `None` while suspended.
    pub(crate) fn add_clock(&mut self, speed: ClockSpeed) -> Option<ClockHandle> {
        match &mut self.output {
            Output::Device(manager) => {
                let mut guard = lock(manager);
                guard.as_mut()?.add_clock(speed).ok()
            }
            Output::Offline(manager) => {
                let mut guard = lock(manager);
                guard.add_clock(speed).ok()
            }
        }
    }

    /// Drains the cpal stream errors the device backend has reported (device changes,
    /// disconnections, buffer errors). kira's cpal backend restarts the output stream itself when
    /// the default device changes or dies, keeping the renderer — and with it all playing sounds
    /// — alive (spec § 4); these errors are surfaced for logging and tooling. Empty on the
    /// offline backend.
    pub(crate) fn take_output_errors(&mut self) -> Vec<String> {
        match &mut self.output {
            Output::Device(manager) => {
                let mut guard = lock(manager);
                let Some(manager) = guard.as_mut() else {
                    return Vec::new();
                };
                let mut errors = Vec::new();
                while let Some(error) = manager.backend_mut().pop_error() {
                    errors.push(error.to_string());
                }
                errors
            }
            Output::Offline(_) => Vec::new(),
        }
    }

    /// Renders `frames` stereo frames from the offline backend. `Err` on the device backend.
    pub(crate) fn render_offline(&mut self, frames: usize) -> Result<Vec<f32>, AudioError> {
        match &mut self.output {
            Output::Offline(manager) => {
                let mut guard = lock(manager);
                Ok(guard.backend_mut().render(frames))
            }
            Output::Device(_) => Err(AudioError::NeedsOffline),
        }
    }
}

impl Output {
    fn offline(sample_rate: u32, limiter: &Arc<LimiterShared>, clock: &Arc<OutputClock>) -> Self {
        Self::offline_with(
            sample_rate,
            kira::Capacities::default().send_track_capacity,
            limiter,
            Some(clock),
        )
    }

    /// An offline output with room for `send_tracks` send tracks (at least kira's default).
    fn offline_with(
        sample_rate: u32,
        send_tracks: usize,
        limiter: &Arc<LimiterShared>,
        clock: Option<&Arc<OutputClock>>,
    ) -> Self {
        let defaults = kira::Capacities::default();
        let manager = match AudioManager::<OfflineBackend>::new(AudioManagerSettings {
            backend_settings: OfflineSettings { sample_rate },
            main_track_builder: main_track(limiter, clock),
            capacities: kira::Capacities {
                send_track_capacity: send_tracks.max(defaults.send_track_capacity),
                ..defaults
            },
            ..AudioManagerSettings::default()
        }) {
            Ok(manager) => manager,
            Err(err) => match err {},
        };
        Self::Offline(Box::new(Mutex::new(manager)))
    }

    fn device(limiter: &Arc<LimiterShared>, clock: &Arc<OutputClock>) -> Option<AudioManager<CpalBackend>> {
        AudioManager::<CpalBackend>::new(AudioManagerSettings {
            backend_settings: CpalBackendSettings::default(),
            main_track_builder: main_track(limiter, Some(clock)),
            ..AudioManagerSettings::default()
        })
        .ok()
    }
}

/// kira's main track with the master limiter: everything the mixer outputs (buses and send
/// buses) passes through it before kira's final clamp.
fn main_track(limiter: &Arc<LimiterShared>, clock: Option<&Arc<OutputClock>>) -> MainTrackBuilder {
    let builder = MainTrackBuilder::new().with_effect(LimiterBuilder(Arc::clone(limiter)));
    match clock {
        Some(clock) => builder.with_effect(OutputClockBuilder(Arc::clone(clock))),
        None => builder,
    }
}

impl<B: kira::backend::Backend> TrackHost for AudioManager<B> {
    fn add_sub_track(
        &mut self,
        builder: kira::track::TrackBuilder,
    ) -> Result<kira::track::TrackHandle, kira::ResourceLimitReached> {
        AudioManager::add_sub_track(self, builder)
    }

    fn add_send_track(
        &mut self,
        builder: kira::track::SendTrackBuilder,
    ) -> Result<kira::track::SendTrackHandle, kira::ResourceLimitReached> {
        AudioManager::add_send_track(self, builder)
    }
}

/// Locks an output-manager mutex, recovering from poisoning: a panic on another thread leaves the
/// mixer safe to use (all state is behind atomics or owned by the audio thread).
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_offline_on_a_device_backend_needs_offline() {
        // A suspended device backend (no stream) needs no audio hardware.
        let mut backend = OutputBackend {
            output: Output::Device(Box::new(Mutex::new(None))),
            limiter: Arc::new(crate::limiter::LimiterShared::default()),
            clock: Arc::default(),
            stashed: None,
            mixdown_limiter: None,
        };
        assert_eq!(backend.render_offline(1), Err(AudioError::NeedsOffline));
    }

    #[test]
    fn the_sample_clock_counts_rendered_frames_and_skips_mixdowns() {
        let settings = AudioSettings {
            headless: true,
            sample_rate: 44_100,
            ..AudioSettings::default()
        };
        let mut backend = OutputBackend::open(&settings);
        assert_eq!(backend.sample_clock(), None, "nothing rendered yet");
        assert!(backend.render_offline(441).is_ok());
        let clock = backend.sample_clock().expect("rendered");
        assert_eq!((clock.frames, clock.sample_rate), (441, 44_100));
        assert_eq!(clock.micros(), 10_000);
        assert!(backend.begin_mixdown(48_000, 1));
        assert!(backend.render_offline(4_800).is_ok());
        assert_eq!(
            backend.sample_clock(),
            None,
            "a mixdown is not on the output's clock"
        );
        assert!(backend.end_mixdown());
        assert_eq!(
            backend.sample_clock(),
            Some(clock),
            "mixdown frames were not counted"
        );
    }

    #[test]
    fn a_suspended_output_has_no_sample_clock() {
        let backend = OutputBackend {
            output: Output::Device(Box::new(Mutex::new(None))),
            limiter: Arc::new(crate::limiter::LimiterShared::default()),
            clock: Arc::new(OutputClock {
                frames: AtomicU64::new(96),
                sample_rate: AtomicU32::new(48_000),
            }),
            stashed: None,
            mixdown_limiter: None,
        };
        assert_eq!(backend.sample_clock(), None);
    }

    #[test]
    fn sample_clock_micros_round_down_and_do_not_overflow() {
        let clock = |frames, sample_rate| SampleClock { frames, sample_rate };
        assert_eq!(clock(1, 48_000).micros(), 20);
        assert_eq!(clock(u64::MAX, 48_000).micros(), u64::MAX);
        assert_eq!(clock(5, 0).micros(), 5_000_000, "a zero rate cannot divide");
    }

    #[test]
    fn a_mixdown_sets_a_device_output_aside_and_puts_it_back() {
        let mut backend = OutputBackend {
            output: Output::Device(Box::new(Mutex::new(None))),
            limiter: Arc::new(crate::limiter::LimiterShared::default()),
            clock: Arc::default(),
            stashed: None,
            mixdown_limiter: None,
        };
        assert!(!backend.end_mixdown(), "nothing to end");
        assert!(backend.begin_mixdown(44_100, 20));
        assert!(backend.in_mixdown() && backend.is_offline());
        assert!(!backend.begin_mixdown(48_000, 1), "one mixdown at a time");
        assert_eq!(backend.render_offline(3).map(|out| out.len()), Ok(6));
        assert!(backend.end_mixdown());
        assert!(
            !backend.in_mixdown() && !backend.is_offline(),
            "the device is back"
        );
        assert_eq!(backend.render_offline(1), Err(AudioError::NeedsOffline));
    }
}
