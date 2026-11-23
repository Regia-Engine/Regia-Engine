// SPDX-License-Identifier: MPL-2.0
//! Voice capture and playback (`docs/specs/16-audio.md` § 3b).
//!
//! [`AudioCapture`] opens a cpal input stream (device selectable), converts whatever the device
//! delivers to mono `f32`, and publishes each callback as one preallocated block. The callback
//! never blocks or allocates. Voice-activity detection (short-term energy threshold
//! with hysteresis and hangover, the ITU-T P.56 active-speech idea), an input level meter and
//! push-to-talk gating run on the game thread in [`AudioCapture::drain`].
//!
//! [`VoiceStreamData`] is the playback side: a kira sound that consumes chunks of decoded frames
//! with a jitter buffer (prebuffer target), linear resampling to the output rate and end-to-end
//! latency measurement (capture timestamp → first sample rendered). Encoding/transport belong to
//! the game; Regia Engine captures and plays back.
//!
//! Android: opening a capture stream needs the `RECORD_AUDIO` permission; request it through
//! the platform layer before calling [`AudioCapture::start`] — Regia Engine never prompts on its own.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};
use kira::{
    Frame,
    info::Info,
    sound::{Sound, SoundData},
};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::AudioError;
use crate::spatial::{
    AudioEmitter, EmitterFrame, ListenerFrame, RolloffCurve, db_to_gain, nearest_listener, spatialize,
};
use crate::voice_chat::{VoicePlacement, pan_gains};

/// Settings of the energy-based voice-activity detector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VadSettings {
    /// RMS level (dBFS) that marks speech onset.
    pub on_threshold_db: f32,
    /// RMS level (dBFS) below which speech ends (hysteresis).
    pub off_threshold_db: f32,
    /// Keep declaring activity this long after the level drops.
    pub hangover: Duration,
    /// Integration window of the RMS.
    pub window: Duration,
}

impl Default for VadSettings {
    fn default() -> Self {
        Self {
            on_threshold_db: -40.0,
            off_threshold_db: -50.0,
            hangover: Duration::from_millis(250),
            window: Duration::from_millis(20),
        }
    }
}

/// Capture settings.
///
/// # Example
///
/// Building this type only configures a capture — it never opens a microphone (that happens in
/// [`AudioCapture::start`]):
///
/// ```
/// use regia_audio::VoiceCaptureSettings;
///
/// let settings = VoiceCaptureSettings::default();
/// assert_eq!(settings.sample_rate, 48_000);
/// assert_eq!(settings.vad.on_threshold_db, -40.0);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceCaptureSettings {
    /// Device name substring to prefer (`None` = default input device).
    pub device: Option<String>,
    /// Requested sample rate; the device's actual rate is reported back and used.
    pub sample_rate: u32,
    /// Ring capacity in seconds (frames older than this are dropped), at most 30 s. Held in full
    /// for device callbacks of at least 64 frames, up to 4 194 304 frames (about 87 s at 48 kHz,
    /// 21.8 s at 192 kHz).
    pub buffer_secs: f32,
    /// VAD configuration.
    pub vad: VadSettings,
}

impl Default for VoiceCaptureSettings {
    fn default() -> Self {
        Self {
            device: None,
            sample_rate: 48_000,
            buffer_secs: 2.0,
            vad: VadSettings::default(),
        }
    }
}

/// Shared capture state, written from the callback (atomics only) and read by the game.
#[derive(Default)]
struct CaptureShared {
    /// Exponentially averaged signal *power* (mono²); `rms` is its square root.
    rms_power: AtomicU32,
    peak: AtomicU32,
    dropped: AtomicU64,
    /// Callback blocks handed to the ring. The device callback and
    /// [`AudioCapture::push_callback_block`] both count one block, not one sample.
    publishes: AtomicU64,
    /// The stream reported a fatal error (the device went away).
    failed: std::sync::atomic::AtomicBool,
    /// The stream reported `DeviceChanged` (a headset switch); the next drain re-opens it.
    device_changed: AtomicBool,
}

/// Records one cpal stream error kind on the shared state (the stream's error callback; atomics
/// only). A `DeviceChanged` (a headset switch) flags a re-open for the next drain: the old stream
/// object stays alive and would read silent forever. Xruns and a denied realtime priority are
/// survivable; anything else marks the capture failed.
fn note_stream_error(shared: &CaptureShared, kind: cpal::ErrorKind) {
    match kind {
        cpal::ErrorKind::DeviceChanged => shared.device_changed.store(true, Ordering::Release),
        cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied => {}
        _ => shared.failed.store(true, Ordering::Release),
    }
}

fn store_f32(slot: &AtomicU32, value: f32) {
    slot.store(value.to_bits(), Ordering::Relaxed);
}

fn load_f32(slot: &AtomicU32) -> f32 {
    f32::from_bits(slot.load(Ordering::Relaxed))
}

fn dbfs(amplitude: f32) -> f32 {
    if amplitude <= 1e-6 {
        -100.0
    } else {
        20.0 * amplitude.log10()
    }
}

/// One step of the level meter's exponential average, in signal **power** (amplitude²). Pure so
/// the audio callback stays tiny and the math is testable: an amplitude/power mix-up here makes
/// the meter climb to a phantom level on silence.
fn meter_step(power: f32, mono: f32) -> f32 {
    power * 0.9 + mono * mono * 0.1
}

/// Samples stored in one ring slot. A longer callback is split across slots.
const SLOT_CAP: usize = 128;
/// Every slot a callback of at least this many samples uses holds at least this many: a callback
/// of `c >= SLOT_CAP / 2` samples takes `ceil(c / SLOT_CAP)` slots, and `c / ceil(c / SLOT_CAP)`
/// is never below `SLOT_CAP / 2`. Sizing the ring at one slot per `MIN_SLOT_FILL` frames of
/// [`VoiceCaptureSettings::buffer_secs`] therefore keeps that many seconds for any callback of
/// at least 64 frames (1.3 ms at 48 kHz), whatever the device's callback length.
const MIN_SLOT_FILL: usize = SLOT_CAP / 2;
/// Most slots in one ring (slot indices are `u16`): 32 MiB of samples. A buffer longer than
/// `MAX_SLOTS × MIN_SLOT_FILL` frames (about 21 s at 192 kHz) keeps that much.
const MAX_SLOTS: usize = 1 << 16;
/// Mono scratch reserved for the device callback. A larger callback grows it once.
const SCRATCH_CAP: usize = 8192;

/// Preallocated single-producer ring of mono blocks. Publishing a block sends one slot index.
struct CaptureRing {
    slots: Vec<Mutex<Vec<f32>>>,
    free_tx: Sender<u16>,
    free_rx: Receiver<u16>,
    ready_tx: Sender<u16>,
    ready_rx: Receiver<u16>,
    scratch: Mutex<Vec<f32>>,
}

impl CaptureRing {
    fn new(buffer_frames: usize) -> Self {
        let slot_count = buffer_frames.div_ceil(MIN_SLOT_FILL).clamp(2, MAX_SLOTS);
        let (free_tx, free_rx) = bounded(slot_count);
        let (ready_tx, ready_rx) = bounded(slot_count);
        let mut slots = Vec::with_capacity(slot_count);
        for index in 0..slot_count {
            let mut samples = Vec::new();
            samples.reserve_exact(SLOT_CAP);
            slots.push(Mutex::new(samples));
            let _ = free_tx.try_send(index as u16);
        }
        let mut scratch = Vec::new();
        scratch.reserve_exact(SCRATCH_CAP);
        let scratch = Mutex::new(scratch);
        // Lock every mutex once here, off the device thread: where std's `Mutex` is a pthread mutex
        // (macOS and the other non-futex Unix targets) its OS lock is boxed on the first lock, which
        // would otherwise allocate inside the device callback (ADR-5550).
        for mutex in slots.iter().chain(std::iter::once(&scratch)) {
            drop(mutex.lock());
        }
        Self {
            slots,
            free_tx,
            free_rx,
            ready_tx,
            ready_rx,
            scratch,
        }
    }

    /// Converts one device callback to mono and publishes it as one block.
    fn push_converted<T: Copy>(
        &self,
        data: &[T],
        channels: usize,
        convert: fn(T) -> f32,
        shared: &CaptureShared,
    ) {
        let mut scratch = {
            let mut guard = self.scratch.lock().unwrap_or_else(|poison| poison.into_inner());
            std::mem::take(&mut *guard)
        };
        scratch.clear();
        let channels = channels.max(1);
        for group in data.chunks(channels) {
            let mono = group.iter().copied().map(convert).sum::<f32>() / group.len() as f32;
            scratch.push(if mono.is_finite() { mono } else { 0.0 });
        }
        self.publish_block(shared, &scratch);
        *self.scratch.lock().unwrap_or_else(|poison| poison.into_inner()) = scratch;
    }

    /// One callback: one publish, split across slots only when it is longer than [`SLOT_CAP`].
    fn publish_block(&self, shared: &CaptureShared, mono: &[f32]) {
        shared.publishes.fetch_add(1, Ordering::Relaxed);
        for chunk in mono.chunks(SLOT_CAP) {
            self.publish_slot(shared, chunk);
        }
    }

    fn publish_slot(&self, shared: &CaptureShared, chunk: &[f32]) {
        let Some(index) = self.take_slot(shared) else {
            for &sample in chunk {
                note_level(shared, if sample.is_finite() { sample } else { 0.0 });
            }
            shared.dropped.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            return;
        };
        let Some(slot) = self.slots.get(index as usize) else {
            shared.dropped.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            return;
        };
        let mut guard = slot.lock().unwrap_or_else(|poison| poison.into_inner());
        guard.clear();
        for &sample in chunk {
            let sample = if sample.is_finite() { sample } else { 0.0 };
            note_level(shared, sample);
            guard.push(sample);
        }
        drop(guard);
        if self.ready_tx.try_send(index).is_err() {
            shared.dropped.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            let _ = self.free_tx.try_send(index);
        }
    }

    /// A free slot, or the oldest ready slot (its samples count as dropped).
    fn take_slot(&self, shared: &CaptureShared) -> Option<u16> {
        if let Ok(index) = self.free_rx.try_recv() {
            return Some(index);
        }
        let Ok(index) = self.ready_rx.try_recv() else {
            return None;
        };
        let len = self
            .slots
            .get(index as usize)
            .map(|slot| slot.lock().unwrap_or_else(|poison| poison.into_inner()).len())
            .unwrap_or(0);
        shared.dropped.fetch_add(len as u64, Ordering::Relaxed);
        Some(index)
    }

    fn drain_into(&self, out: &mut Vec<f32>) {
        while let Ok(index) = self.ready_rx.try_recv() {
            if let Some(slot) = self.slots.get(index as usize) {
                let guard = slot.lock().unwrap_or_else(|poison| poison.into_inner());
                out.extend_from_slice(&guard);
            }
            let _ = self.free_tx.try_send(index);
        }
    }
}

fn note_level(shared: &CaptureShared, mono: f32) {
    let abs = mono.abs();
    if abs > load_f32(&shared.peak) {
        store_f32(&shared.peak, abs);
    }
    let power = load_f32(&shared.rms_power);
    store_f32(&shared.rms_power, meter_step(power, mono));
}

/// Everything [`AudioCapture`] needs from one successful `open`.
struct OpenStream {
    stream: cpal::Stream,
    device: String,
    sample_rate: u32,
    channels: u16,
    ring: Arc<CaptureRing>,
    shared: Arc<CaptureShared>,
}

fn open_stream(settings: &VoiceCaptureSettings) -> Result<OpenStream, AudioError> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = match &settings.device {
        None => host
            .default_input_device()
            .ok_or_else(|| AudioError::Capture("no default input device".into()))?,
        Some(want) => host
            .input_devices()
            .ok()
            .and_then(|devices| {
                devices
                    .map(|d| {
                        let name = d.to_string();
                        (d, name)
                    })
                    .find(|(_, name)| name.contains(want.as_str()))
                    .map(|(d, _)| d)
            })
            .ok_or_else(|| AudioError::Capture(format!("no input device matching `{want}`")))?,
    };
    let device_name = device.to_string();
    let default_config = device
        .default_input_config()
        .map_err(|e| AudioError::Capture(format!("input config: {e}")))?;
    // A device that cannot run at the requested rate keeps its own: the voice chat converter and
    // the loopback stream resample whatever arrives.
    let requested_supported = device.supported_input_configs().is_ok_and(|mut ranges| {
        ranges.any(|r| (r.min_sample_rate()..=r.max_sample_rate()).contains(&settings.sample_rate))
    });
    let sample_rate = if requested_supported {
        settings.sample_rate
    } else {
        default_config.sample_rate()
    };
    let config = cpal::StreamConfig {
        channels: default_config.channels().min(2),
        sample_rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let ring = Arc::new(CaptureRing::new(capture_buffer_frames(
        settings.buffer_secs,
        sample_rate,
    )));
    let shared = Arc::new(CaptureShared::default());

    let callback_shared = Arc::clone(&shared);
    let callback_ring = Arc::clone(&ring);
    let channels = config.channels.max(1) as usize;

    macro_rules! convert_and_push {
        ($ty:ty, $convert:expr) => {{
            let shared = Arc::clone(&callback_shared);
            let error_shared = Arc::clone(&callback_shared);
            let ring = Arc::clone(&callback_ring);
            device.build_input_stream(
                config.clone(),
                move |data: &[$ty], _: &cpal::InputCallbackInfo| {
                    ring.push_converted(data, channels, $convert, &shared);
                },
                move |error: cpal::Error| {
                    tracing::warn!(target: "regia_audio", "capture error: {error}");
                    note_stream_error(&error_shared, error.kind());
                },
                None,
            )
        }};
    }

    let stream = match default_config.sample_format() {
        cpal::SampleFormat::F32 => convert_and_push!(f32, |s| s),
        cpal::SampleFormat::I16 => convert_and_push!(i16, |s| s as f32 / 32_768.0),
        cpal::SampleFormat::U16 => convert_and_push!(u16, |s| (s as f32 - 32_768.0) / 32_768.0),
        other => {
            return Err(AudioError::Capture(format!(
                "unsupported input sample format {other:?}"
            )));
        }
    }
    .map_err(|e| AudioError::Capture(format!("input stream: {e}")))?;
    stream
        .play()
        .map_err(|e| AudioError::Capture(format!("could not start capture: {e}")))?;
    tracing::info!(target: "regia_audio", device = %device_name, "voice capture started");

    Ok(OpenStream {
        stream,
        device: device_name,
        sample_rate: config.sample_rate,
        channels: config.channels,
        ring,
        shared,
    })
}

/// An open input stream. [`AudioCapture::stop`] releases the microphone; `resume` re-opens it.
pub struct AudioCapture {
    stream: Mutex<Option<cpal::Stream>>,
    device: String,
    ring: Arc<CaptureRing>,
    shared: Arc<CaptureShared>,
    settings: VoiceCaptureSettings,
    actual_rate: u32,
    actual_channels: u16,
    vad: VoiceActivity,
    talking: bool,
    /// Frames the ring dropped before the current stream opened (the count carries across a
    /// stop/resume cycle, so `dropped_frames` keeps counting "since it started").
    dropped_base: u64,
}

impl AudioCapture {
    /// Opens the input device and starts capturing. Errors — no device, busy device, no
    /// permission — come back as [`AudioError::Capture`]; nothing panics.
    pub fn start(settings: &VoiceCaptureSettings) -> Result<Self, AudioError> {
        let opened = open_stream(settings)?;
        Ok(Self {
            stream: Mutex::new(Some(opened.stream)),
            device: opened.device,
            ring: opened.ring,
            shared: opened.shared,
            settings: settings.clone(),
            actual_rate: opened.sample_rate,
            actual_channels: opened.channels,
            vad: VoiceActivity::with_sample_rate(settings.vad.clone(), opened.sample_rate),
            talking: true,
            dropped_base: 0,
        })
    }

    /// Stops the stream and releases the microphone. Restart with [`AudioCapture::resume`].
    pub fn stop(&mut self) {
        *self.stream.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// Re-opens the stream after [`AudioCapture::stop`] (or an OS suspend). The ring and level
    /// state keep running; a no-op when already capturing. The dropped-frame counter carries
    /// across the restart: `dropped_frames` counts since capture started, not since the
    /// current stream opened.
    pub fn resume(&mut self) -> Result<(), AudioError> {
        if self.is_running() {
            return Ok(());
        }
        let opened = open_stream(&self.settings)?;
        // Carry the pre-restart dropped count into the base before the swap, so `dropped_frames`
        // counts since capture started. `stop`/`resume` run on the game thread (`&mut self`),
        // so the stream cannot change between the check and the store.
        self.stash_dropped();
        let mut guard = self.stream.lock().unwrap_or_else(|p| p.into_inner());
        self.actual_rate = opened.sample_rate;
        self.actual_channels = opened.channels;
        self.device = opened.device;
        // The new stream feeds its own ring and meter; keep draining the old ones and the
        // microphone would read as silent after every suspend/resume.
        self.ring = opened.ring;
        self.shared = opened.shared;
        self.vad.set_sample_rate(opened.sample_rate);
        *guard = Some(opened.stream);
        Ok(())
    }

    /// True while the stream is open.
    pub fn is_running(&self) -> bool {
        self.stream.lock().unwrap_or_else(|p| p.into_inner()).is_some()
    }

    /// Moves the current stream's dropped count into the cross-restart total. Called before
    /// the stream is swapped, so the running counter belongs to the new stream alone.
    fn stash_dropped(&mut self) {
        self.dropped_base += self.shared.dropped.load(Ordering::Relaxed);
    }

    /// The device name actually opened.
    pub fn device(&self) -> &str {
        &self.device
    }

    /// The sample rate frames are delivered at (the device's, which may differ from the request).
    pub fn sample_rate(&self) -> u32 {
        self.actual_rate
    }

    /// The channel count of the opened stream (deliveries are downmixed to mono regardless).
    pub fn channels(&self) -> u16 {
        self.actual_channels
    }

    /// A capture that never opens a device. [`Self::push_callback_block`] feeds samples the way
    /// the microphone callback would, so tests can drive the ring without a host device.
    pub fn headless(settings: &VoiceCaptureSettings) -> Self {
        let ring = Arc::new(CaptureRing::new(capture_buffer_frames(
            settings.buffer_secs,
            settings.sample_rate,
        )));
        Self {
            stream: Mutex::new(None),
            device: String::new(),
            ring,
            shared: Arc::new(CaptureShared::default()),
            settings: settings.clone(),
            actual_rate: settings.sample_rate,
            actual_channels: 0,
            vad: VoiceActivity::with_sample_rate(settings.vad.clone(), settings.sample_rate),
            talking: true,
            dropped_base: 0,
        }
    }

    /// Injects one callback block of mono samples.
    ///
    /// The block is one publish: the game thread later drains the samples with no further
    /// per-sample traffic. Non-finite samples are stored as silence. A full ring drops the oldest
    /// block (its samples count in [`Self::dropped_frames`]).
    pub fn push_callback_block(&self, mono: &[f32]) {
        self.ring.publish_block(&self.shared, mono);
    }

    /// Callback blocks published since this capture was created.
    pub fn callback_blocks_published(&self) -> u64 {
        self.shared.publishes.load(Ordering::Relaxed)
    }

    /// Drains all frames captured since the last call, updating the level meter and VAD. `dt` is
    /// the time since the last drain (hangover bookkeeping).
    pub fn drain(&mut self, dt: f64) -> Vec<f32> {
        self.recover_device_change();
        let mut frames = Vec::new();
        self.ring.drain_into(&mut frames);
        self.vad.process(&frames, dt);
        // Reset the peak so the next window reports its own maximum.
        store_f32(&self.shared.peak, 0.0);
        frames
    }

    /// Appends every captured sample to `out` without running the meter's VAD (voice chat runs its
    /// own frame-based detector).
    pub(crate) fn drain_into(&mut self, out: &mut Vec<f32>) {
        self.recover_device_change();
        self.ring.drain_into(out);
        store_f32(&self.shared.peak, 0.0);
    }

    /// True once the stream reported a fatal error (device unplugged, host gone).
    pub(crate) fn stream_failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }

    /// Re-opens the stream after its device changed (deep-zai audio-mix finding 13), on the game
    /// thread at the next drain. A stopped capture (suspended output) only forgets the change:
    /// `resume` opens afresh, and the microphone must stay released until then.
    fn recover_device_change(&mut self) {
        if !self.is_running() {
            self.shared.device_changed.store(false, Ordering::Release);
            return;
        }
        let _ = self.recover_device_change_with(|capture| {
            capture.stop();
            capture.resume()
        });
    }

    /// Handles a pending `DeviceChanged` with `reopen` (the real one stops and resumes the
    /// stream on the new default or matching device). `None` when no change is pending. A failed
    /// re-open leaves the capture stopped and failed (`stream_failed`; voice chat reads
    /// `NoInputDevice`), so it reads closed instead of silently open.
    pub(crate) fn recover_device_change_with(
        &mut self,
        reopen: impl FnOnce(&mut Self) -> Result<(), AudioError>,
    ) -> Option<Result<(), AudioError>> {
        if !self.shared.device_changed.swap(false, Ordering::AcqRel) {
            return None;
        }
        let result = reopen(self);
        match &result {
            Ok(()) => {
                tracing::info!(target: "regia_audio", device = %self.device, "voice capture re-opened after a device change");
            }
            Err(error) => {
                tracing::warn!(target: "regia_audio", "voice capture closed: the input device changed and did not re-open ({error})");
                self.stop();
                self.shared.failed.store(true, Ordering::Release);
            }
        }
        Some(result)
    }

    /// Like [`AudioCapture::drain`] but returns the frames only while push-to-talk is active
    /// (frames are consumed either way, so the meter and VAD keep running).
    pub fn drain_talking(&mut self, dt: f64) -> Vec<f32> {
        let frames = self.drain(dt);
        if self.talking { frames } else { Vec::new() }
    }

    /// Sets the push-to-talk gate.
    pub fn set_talking(&mut self, talking: bool) {
        self.talking = talking;
    }

    /// True while the push-to-talk gate is open.
    pub fn is_talking(&self) -> bool {
        self.talking
    }

    /// The input level since the last drain: `(rms, peak)` as amplitude 0..1.
    pub fn level(&self) -> (f32, f32) {
        (
            load_f32(&self.shared.rms_power).sqrt(),
            load_f32(&self.shared.peak),
        )
    }

    /// The input level in dBFS: `(rms, peak)`.
    pub fn level_db(&self) -> (f32, f32) {
        let (rms, peak) = self.level();
        (dbfs(rms), dbfs(peak))
    }

    /// Frames dropped by the ring since capture started (across restarts).
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_base + self.shared.dropped.load(Ordering::Relaxed)
    }

    /// True while the VAD considers the input speech.
    pub fn is_voice_active(&self) -> bool {
        self.vad.is_active()
    }

    /// Builds the capture in its stopped state without opening a device — the state
    /// [`AudioCapture::stop`] (and `Audio::suspend_output`) leaves. Lets the suspended-state
    /// accessor semantics be tested on a host with no microphone.
    #[cfg(test)]
    pub(crate) fn stopped_for_test(settings: &VoiceCaptureSettings) -> Self {
        Self::headless(settings)
    }
}

/// Energy-based voice activity: short-term RMS threshold with hysteresis and hangover.
#[derive(Debug, Clone)]
pub struct VoiceActivity {
    settings: VadSettings,
    window: Vec<f32>,
    fill: usize,
    sum_sq: f32,
    active: bool,
    quiet_for: Duration,
}

/// Longest voice-activity window honored, seconds.
const MAX_VAD_WINDOW_SECS: f64 = 10.0;

/// Longest capture buffer honored, seconds.
const MAX_CAPTURE_BUFFER_SECS: f32 = 30.0;

/// Frames of capture ring for `buffer_secs` at `sample_rate`: at least 1, and capped so an
/// infinite or huge setting cannot overflow the channel's capacity (which aborts).
fn capture_buffer_frames(buffer_secs: f32, sample_rate: u32) -> usize {
    let secs = if buffer_secs.is_nan() {
        0.0
    } else {
        buffer_secs.clamp(0.0, MAX_CAPTURE_BUFFER_SECS)
    };
    ((f64::from(secs) * f64::from(sample_rate)) as usize).max(1)
}

impl VoiceActivity {
    /// A detector with `settings` (the window runs at a nominal 48 kHz).
    pub fn new(settings: VadSettings) -> Self {
        Self::with_sample_rate(settings, 48_000)
    }

    /// A detector whose window is sized for frames arriving at `sample_rate` Hz.
    pub fn with_sample_rate(settings: VadSettings, sample_rate: u32) -> Self {
        let window_len = Self::window_len(&settings, sample_rate);
        Self {
            settings,
            window: vec![0.0; window_len],
            fill: 0,
            sum_sq: 0.0,
            active: false,
            quiet_for: Duration::ZERO,
        }
    }

    fn window_len(settings: &VadSettings, sample_rate: u32) -> usize {
        // A float-to-int cast saturates, so an absurd window would ask for an impossible Vec.
        let secs = settings.window.as_secs_f64().min(MAX_VAD_WINDOW_SECS);
        ((secs * f64::from(sample_rate)) as usize).max(1)
    }

    /// Re-sizes the window for a new frame rate (the device rate changed on resume). The window
    /// restarts empty; the active/hangover state is kept.
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let len = Self::window_len(&self.settings, sample_rate);
        if len != self.window.len() {
            self.window = vec![0.0; len];
            self.fill = 0;
            self.sum_sq = 0.0;
        }
    }

    /// The window length in frames.
    pub fn window_frames(&self) -> usize {
        self.window.len()
    }

    /// Feeds one block of frames; `dt` is the block duration (hangover bookkeeping).
    pub fn process(&mut self, frames: &[f32], dt: f64) {
        for &frame in frames {
            // A non-finite sample would poison the running sum for good.
            let frame = if frame.is_finite() { frame } else { 0.0 };
            let oldest = self.window[self.fill];
            self.sum_sq += frame * frame - oldest * oldest;
            self.window[self.fill] = frame;
            self.fill = (self.fill + 1) % self.window.len();
        }
        let rms = (self.sum_sq.max(0.0) / self.window.len() as f32).sqrt();
        let level_db = dbfs(rms);
        if self.active {
            if level_db < self.settings.off_threshold_db {
                self.quiet_for += Duration::try_from_secs_f64(dt).unwrap_or(Duration::ZERO);
                if self.quiet_for >= self.settings.hangover {
                    self.active = false;
                }
            } else {
                self.quiet_for = Duration::ZERO;
            }
        } else if level_db > self.settings.on_threshold_db {
            self.active = true;
            self.quiet_for = Duration::ZERO;
        }
    }

    /// True while the input counts as speech.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The current window RMS in dBFS.
    pub fn level_db(&self) -> f32 {
        let rms = (self.sum_sq.max(0.0) / self.window.len() as f32).sqrt();
        dbfs(rms)
    }
}

/// A reference to one chunk of captured/decoded frames on its way to the playback stream.
pub struct VoiceChunk {
    /// Mono frames.
    pub samples: Arc<[f32]>,
    /// When the first frame was captured (loopback latency measurement).
    pub captured_at: Option<Instant>,
}

/// The placement of a voice stream, shared between the game thread (the
/// [`VoiceStreamHandle`] writes it, once per `AudioSync` for an anchored stream) and the audio
/// thread (the sound reads it once per processing batch). A seqlock over packed fields: the
/// single writer is the game thread and the read is a handful of atomic loads, so the audio
/// thread never blocks behind a preempted game thread the way locking a `Mutex` in kira's
/// `process` could (the mixer would stall). Public-domain technique (Linux kernel `seqlock_t`,
/// Wikipedia "Seqlock"); original code.
pub struct SharedPlacement {
    /// Even while idle, odd while a store is in flight; bumped around every store.
    seq: AtomicU32,
    /// `TAG_*`: which variant the fields below hold.
    tag: AtomicU32,
    /// `f32` bits of the spatial distance gain.
    gain: AtomicU32,
    /// `f32` bits of the spatial pan.
    pan: AtomicU32,
    /// `f64` bits of the spatial low-pass cutoff; [`NO_CUTOFF`] = none.
    cutoff: AtomicU64,
}

/// [`SharedPlacement::tag`] value for [`VoicePlacement::Center`].
const TAG_CENTER: u32 = 0;
/// [`SharedPlacement::tag`] value for [`VoicePlacement::Unresolved`].
const TAG_UNRESOLVED: u32 = 1;
/// [`SharedPlacement::tag`] value for [`VoicePlacement::Spatial`].
const TAG_SPATIAL: u32 = 2;
/// [`SharedPlacement::cutoff`] sentinel for "no cutoff" (a real cutoff is positive, so its bits
/// are never all ones).
const NO_CUTOFF: u64 = u64::MAX;

impl SharedPlacement {
    /// The flat default ([`VoicePlacement::Center`]).
    fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            tag: AtomicU32::new(TAG_CENTER),
            gain: AtomicU32::new(0),
            pan: AtomicU32::new(0),
            cutoff: AtomicU64::new(NO_CUTOFF),
        }
    }

    /// Publishes `placement`. One writer at a time (the game thread); stores the packed fields
    /// under an odd sequence and releases it back to even.
    fn store(&self, placement: VoicePlacement) {
        let (tag, gain, pan, cutoff) = match placement {
            VoicePlacement::Center => (TAG_CENTER, 0.0_f32, 0.0_f32, NO_CUTOFF),
            VoicePlacement::Unresolved => (TAG_UNRESOLVED, 0.0, 0.0, NO_CUTOFF),
            VoicePlacement::Spatial { gain, pan, cutoff_hz } => {
                (TAG_SPATIAL, gain, pan, cutoff_hz.map_or(NO_CUTOFF, f64::to_bits))
            }
        };
        self.seq.fetch_add(1, Ordering::Acquire);
        // The release fence pairs with the reader's acquire fence: a reader that sees any field
        // stored below also sees the odd sequence (Boehm, "Can seqlocks get along with programming
        // language memory models?", MSPC 2012, the fence-based writer; integ-v06h, ADR-7762 (2)).
        fence(Ordering::Release);
        self.tag.store(tag, Ordering::Relaxed);
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
        self.pan.store(pan.to_bits(), Ordering::Relaxed);
        self.cutoff.store(cutoff, Ordering::Relaxed);
        self.seq.fetch_add(1, Ordering::Release);
    }

    /// The placement published most recently: re-reads while a store is in flight.
    fn load(&self) -> VoicePlacement {
        loop {
            let seen = self.seq.load(Ordering::Acquire);
            let tag = self.tag.load(Ordering::Relaxed);
            let gain = self.gain.load(Ordering::Relaxed);
            let pan = self.pan.load(Ordering::Relaxed);
            let cutoff = self.cutoff.load(Ordering::Relaxed);
            // The kernel reader's `smp_rmb()`: the field loads above may not be satisfied after
            // the second sequence load, or a store landing between them could validate a torn
            // placement on a weakly ordered CPU (Android aarch64; fixver-zai-04 finding 1).
            fence(Ordering::Acquire);
            if seen.is_multiple_of(2) && seen == self.seq.load(Ordering::Acquire) {
                return match tag {
                    TAG_CENTER => VoicePlacement::Center,
                    TAG_UNRESOLVED => VoicePlacement::Unresolved,
                    _ => VoicePlacement::Spatial {
                        gain: f32::from_bits(gain),
                        pan: f32::from_bits(pan),
                        cutoff_hz: (cutoff != NO_CUTOFF).then(|| f64::from_bits(cutoff)),
                    },
                };
            }
            std::hint::spin_loop();
        }
    }
}

/// The `(left, right)` target gains and the low-pass cutoff of a placement. The gains are the
/// shared equal-power (sqrt) pan law of [`crate::voice_chat`]'s `pan_gains` (center 1, 1),
/// times the distance gain, so a fix to the pan law applies to both mixers. A non-finite gain
/// is silence, never a NaN in the Voice bus.
fn placement_target(placement: VoicePlacement) -> ([f32; 2], Option<f64>) {
    match placement {
        VoicePlacement::Center => (pan_gains(1.0, 0.0), None),
        VoicePlacement::Unresolved => (
            pan_gains(db_to_gain(crate::voice_chat::FLAT_UNRESOLVED_DB), 0.0),
            None,
        ),
        VoicePlacement::Spatial { gain, pan, cutoff_hz } => {
            let gain = if gain.is_finite() {
                gain.clamp(0.0, 1.0)
            } else {
                0.0
            };
            (pan_gains(gain, pan), cutoff_hz)
        }
    }
}

/// Playback-side statistics (atomics; written by the audio thread, read by the game).
#[derive(Default)]
pub struct VoiceStreamStats {
    buffered_frames: AtomicU64,
    last_latency_ms: AtomicU64,
    max_latency_ms: AtomicU64,
    chunks_pushed: AtomicU64,
    chunks_dropped: AtomicU64,
    running: AtomicBool,
}

/// A snapshot of [`VoiceStreamStats`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VoiceStreamSnapshot {
    /// Frames currently jitter-buffered.
    pub buffered_frames: u64,
    /// Wall time from capture to first render of the most recent chunk, milliseconds.
    pub last_latency_ms: u64,
    /// Maximum observed latency, milliseconds.
    pub max_latency_ms: u64,
    /// Chunks handed to `push`, queued or not (the refused ones are also in `chunks_dropped`).
    pub chunks_pushed: u64,
    /// Chunks dropped because the jitter buffer was full: refused by `push`, or the oldest queued
    /// chunk the playback side discarded to make room.
    pub chunks_dropped: u64,
    /// True while the stream is running.
    pub running: bool,
}

/// A kira sound data that plays [`VoiceChunk`]s with jitter buffering and linear resampling.
pub struct VoiceStreamData {
    sender: Sender<VoiceChunk>,
    receiver: Receiver<VoiceChunk>,
    stats: Arc<VoiceStreamStats>,
    placement: Arc<SharedPlacement>,
    prebuffer_frames: usize,
    capture_rate: f64,
}

/// The game-side control of a playing voice stream: push chunks, read stats, stop.
pub struct VoiceStreamHandle {
    sender: Sender<VoiceChunk>,
    stats: Arc<VoiceStreamStats>,
    placement: Arc<SharedPlacement>,
}

impl VoiceStreamData {
    /// Creates the data (keep the handle from `into_sound`; this constructor is used by
    /// [`crate::Audio::start_voice_loopback`]). `prebuffer_ms` is the jitter-buffer target
    /// before playback starts; `capture_rate` is the rate chunks were captured at. The stream
    /// starts unplaced ([`VoicePlacement::Center`]: flat, centred); see
    /// [`VoiceStreamHandle::set_placement`] and [`VoiceStreamAnchor`].
    pub fn new(prebuffer_ms: u32, capture_rate: u32) -> Self {
        let (sender, receiver) = bounded(128);
        Self {
            sender,
            receiver,
            stats: Arc::new(VoiceStreamStats {
                running: AtomicBool::new(true),
                ..VoiceStreamStats::default()
            }),
            placement: Arc::new(SharedPlacement::new()),
            prebuffer_frames: (f64::from(prebuffer_ms) / 1_000.0 * f64::from(capture_rate)) as usize,
            // A zero rate would freeze the read cursor and repeat one sample forever.
            capture_rate: f64::from(capture_rate.max(1)),
        }
    }

    /// A handle to push chunks before the sound is handed to the manager (the play handle and
    /// this one share the channel, stats and placement).
    pub fn early_handle(&self) -> VoiceStreamHandle {
        VoiceStreamHandle {
            sender: self.sender.clone(),
            stats: Arc::clone(&self.stats),
            placement: Arc::clone(&self.placement),
        }
    }
}

impl SoundData for VoiceStreamData {
    type Error = Infallible;
    type Handle = VoiceStreamHandle;

    fn into_sound(mut self) -> Result<(Box<dyn Sound>, Self::Handle), Self::Error> {
        let handle = VoiceStreamHandle {
            sender: std::mem::replace(&mut self.sender, bounded(1).0),
            stats: Arc::clone(&self.stats),
            placement: Arc::clone(&self.placement),
        };
        Ok((
            Box::new(VoiceStreamSound {
                receiver: self.receiver,
                stats: self.stats,
                placement: self.placement,
                prebuffer_frames: self.prebuffer_frames,
                capture_rate: self.capture_rate,
                queue: VecDeque::with_capacity(128),
                current: None,
                carry: 0.0,
                primed: false,
                gains: [1.0, 1.0],
                low_pass: 0.0,
            }),
            handle,
        ))
    }
}

impl VoiceStreamHandle {
    /// Pushes decoded frames. `captured_at` timestamps the chunk for latency measurement
    /// (pass the capture time; `None` skips the measurement for that chunk).
    pub fn push(&self, samples: Arc<[f32]>, captured_at: Option<Instant>) {
        let _ = self.stats.chunks_pushed.fetch_add(1, Ordering::Relaxed);
        if self.sender.try_send(VoiceChunk { samples, captured_at }).is_err() {
            let _ = self.stats.chunks_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Stops the stream (the sound ends on the next processing batch).
    pub fn stop(&self) {
        self.stats.running.store(false, Ordering::Relaxed);
    }

    /// Places the stream in the stereo field (spec 16 § 3b, optional 3D positioning): distance
    /// gain, pan −1..1 and an optional air-absorption low-pass.
    /// [`VoicePlacement::Center`] restores the flat default. The gains ramp to a new placement
    /// over one processing batch, so a per-frame update from a [`VoiceStreamAnchor`] never
    /// clicks; the batch after the update holds it exactly.
    pub fn set_placement(&self, placement: VoicePlacement) {
        self.placement.store(placement);
    }

    /// The placement set most recently ([`VoicePlacement::Center`] until placed).
    #[must_use]
    pub fn placement(&self) -> VoicePlacement {
        self.placement.load()
    }

    /// Statistics snapshot.
    pub fn stats(&self) -> VoiceStreamSnapshot {
        VoiceStreamSnapshot {
            buffered_frames: self.stats.buffered_frames.load(Ordering::Relaxed),
            last_latency_ms: self.stats.last_latency_ms.load(Ordering::Relaxed),
            max_latency_ms: self.stats.max_latency_ms.load(Ordering::Relaxed),
            chunks_pushed: self.stats.chunks_pushed.load(Ordering::Relaxed),
            chunks_dropped: self.stats.chunks_dropped.load(Ordering::Relaxed),
            running: self.stats.running.load(Ordering::Relaxed),
        }
    }
}

/// The kira sound: consumes chunks with a jitter buffer, linearly resamples to the output rate
/// and pans/attenuates per the shared placement.
struct VoiceStreamSound {
    receiver: Receiver<VoiceChunk>,
    stats: Arc<VoiceStreamStats>,
    placement: Arc<SharedPlacement>,
    prebuffer_frames: usize,
    capture_rate: f64,
    queue: VecDeque<VoiceChunk>,
    current: Option<(Arc<[f32]>, f64)>,
    carry: f64,
    primed: bool,
    /// The `(left, right)` gains of the last rendered sample (start flat).
    gains: [f32; 2],
    /// The one-pole low-pass state for an air-absorption cutoff.
    low_pass: f32,
}

impl VoiceStreamSound {
    fn record_latency(&self, captured_at: Instant) {
        let ms = Instant::now().duration_since(captured_at).as_millis() as u64;
        let _ = self.stats.last_latency_ms.swap(ms, Ordering::Relaxed);
        let mut max = self.stats.max_latency_ms.load(Ordering::Relaxed);
        while ms > max {
            match self
                .stats
                .max_latency_ms
                .compare_exchange(max, ms, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(actual) => max = actual,
            }
        }
    }

    /// Reads one resampled sample, advancing the input cursor by `step` capture samples.
    fn read_sample(&mut self, step: f64) -> f32 {
        loop {
            let Some((samples, pos)) = self.current.as_mut() else {
                let Some(chunk) = self.queue.pop_front() else {
                    return 0.0;
                };
                if let Some(captured) = chunk.captured_at {
                    self.record_latency(captured);
                }
                self.current = Some((chunk.samples, self.carry));
                self.carry = 0.0;
                continue;
            };
            // The chunks are one continuous stream: a chunk lasts `len` input periods and its
            // last sample interpolates toward the next chunk's first (held when none is queued
            // yet). Only the overshoot past `len` carries into the next chunk, so no sample and
            // no period is dropped at a chunk edge (audit-gameplay H2, ADR-1809).
            let len = samples.len() as f64;
            if *pos >= len {
                self.carry = *pos - len;
                self.current = None;
                continue;
            }
            let index = *pos as usize;
            let frac = (*pos - index as f64) as f32;
            let s0 = samples[index];
            let s1 = samples
                .get(index + 1)
                .or_else(|| self.queue.front().and_then(|next| next.samples.first()))
                .copied()
                .unwrap_or(s0);
            let out = s0 + (s1 - s0) * frac;
            *pos += step;
            return out;
        }
    }
}

impl Sound for VoiceStreamSound {
    fn process(&mut self, out: &mut [Frame], dt: f64, _info: &Info) {
        self.render(out, dt);
    }

    fn finished(&self) -> bool {
        !self.stats.running.load(Ordering::Relaxed)
    }
}

impl VoiceStreamSound {
    /// Renders one batch (kira's [`Sound::process`] body, split out so tests drive it without a
    /// kira `Info`).
    fn render(&mut self, out: &mut [Frame], dt: f64) {
        // Drain the channel into the jitter queue (capacity pre-reserved: no allocation).
        while let Ok(chunk) = self.receiver.try_recv() {
            if self.queue.len() >= self.queue.capacity() {
                // Full: drop the oldest queued chunk (keeps latency bounded) and count it.
                self.queue.pop_front();
                let _ = self.stats.chunks_dropped.fetch_add(1, Ordering::Relaxed);
            }
            self.queue.push_back(chunk);
        }
        let buffered: usize = self.queue.iter().map(|c| c.samples.len()).sum();

        if !self.primed && buffered < self.prebuffer_frames {
            // Jitter buffer still filling: stay silent.
            out.fill(Frame::ZERO);
            self.stats
                .buffered_frames
                .store(buffered as u64, Ordering::Relaxed);
            return;
        }
        self.primed = true;

        let step = dt * self.capture_rate;
        // 3D positioning (spec 16 § 3b): read the placement once per batch and ramp the gains
        // linearly to its target, so the batch's last sample sits on the target and the next
        // batch holds it exactly.
        let (target, cutoff_hz) = placement_target(self.placement.load());
        let n = out.len().max(1) as f32;
        let gstep = [(target[0] - self.gains[0]) / n, (target[1] - self.gains[1]) / n];
        // One-pole low-pass: a = 1 - e^(-2πfc/rate), the same coefficient the voice-chat speaker
        // mixer uses. `dt` is the time one output frame stands for (kira documents it as "the
        // time between each frame"), so the stream's sample rate is 1/dt — independent of how
        // many frames this batch carries; a cutoff at or above Nyquist passes through.
        let rate = if dt > 0.0 { 1.0 / dt } else { 0.0 };
        let alpha = cutoff_hz
            .filter(|fc| *fc > 0.0 && *fc < rate * 0.5)
            .map(|fc| 1.0 - (-std::f32::consts::TAU * fc as f32 / rate as f32).exp());
        for frame in out.iter_mut() {
            // Remote voice is untrusted data: a NaN would stay in the Voice bus's effects for
            // good, a huge value would blast the listener. Normalised PCM is −1..1.
            let sample = self.read_sample(step);
            let sample = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            let wet = match alpha {
                Some(a) => {
                    self.low_pass += a * (sample - self.low_pass);
                    self.low_pass
                }
                None => {
                    self.low_pass = sample;
                    sample
                }
            };
            self.gains[0] += gstep[0];
            self.gains[1] += gstep[1];
            *frame = Frame::new(wet * self.gains[0], wet * self.gains[1]);
        }

        let queued: usize = self.queue.iter().map(|c| c.samples.len()).sum();
        let current_left = self
            .current
            .as_ref()
            .map_or(0, |(s, p)| (s.len() as f64 - p).max(0.0) as usize);
        self.stats
            .buffered_frames
            .store((queued + current_left) as u64, Ordering::Relaxed);
    }
}

/// Anchors the voice loopback stream to an entity (spec 16 § 3b): while this component is in the
/// world, the running [`VoiceStreamData`] stream pans and attenuates like an [`AudioEmitter`]
/// at the entity's position against the nearest listener (the `AudioSync` pass updates the
/// placement each frame). The loopback is one stream, so the first anchor by entity id wins.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct VoiceStreamAnchor {
    /// Full volume inside this distance, metres.
    pub min_distance: f32,
    /// Silence past this distance, metres.
    pub max_distance: f32,
    /// How gain falls with distance between the two.
    pub rolloff: RolloffCurve,
    /// Air absorption: a farther stream loses its highs (a per-distance low-pass cutoff).
    pub air_absorption: bool,
}

impl Default for VoiceStreamAnchor {
    fn default() -> Self {
        Self {
            min_distance: 2.0,
            max_distance: 30.0,
            rolloff: RolloffCurve::Log { db_per_doubling: 6.0 },
            air_absorption: false,
        }
    }
}

impl VoiceStreamAnchor {
    /// The emitter acoustics the anchor gives the stream: the Voice bus, no autoplay, no cone
    /// and no Doppler (a decoded stream has no authored cone; the entity's motion must not
    /// pitch-shift speech).
    #[must_use]
    pub fn emitter_params(&self) -> AudioEmitter {
        AudioEmitter {
            autoplay: false,
            looping: false,
            min_distance: self.min_distance,
            max_distance: self.max_distance,
            rolloff: self.rolloff.clone(),
            doppler_factor: 0.0,
            occlusion: false,
            air_absorption: self.air_absorption,
            bus: Some("Voice".to_string()),
            ..AudioEmitter::default()
        }
    }
}

/// Where the voice loopback stream plays for an anchor at `position` against `listeners`
/// (spec 16 § 3b): the emitter math of [`spatialize`] against the nearest listener. With no
/// listener the placement stays [`VoicePlacement::Center`] (flat, not silent); past
/// `max_distance` the gain is 0, like an emitter out of range.
#[must_use]
pub fn stream_placement(
    anchor: &VoiceStreamAnchor,
    position: Vec3,
    listeners: &[ListenerFrame],
) -> VoicePlacement {
    let Some(index) = nearest_listener(listeners, position) else {
        return VoicePlacement::Center;
    };
    let frame = EmitterFrame {
        position,
        velocity: Vec3::ZERO,
        forward: Vec3::NEG_Z,
    };
    let spatial = spatialize(&listeners[index], &frame, &anchor.emitter_params());
    VoicePlacement::Spatial {
        gain: spatial.gain,
        pan: spatial.pan,
        cutoff_hz: spatial.cutoff_hz,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closed gate consumes the ring (the meter and VAD still see the samples) and returns none.
    #[test]
    fn closed_gate_discards_captured_frames() {
        let mut capture = AudioCapture::headless(&VoiceCaptureSettings::default());
        capture.push_callback_block(&[0.5]);
        capture.set_talking(false);
        assert!(
            capture.drain_talking(1.0 / 60.0).is_empty(),
            "a closed gate returns no frames"
        );
        capture.set_talking(true);
        assert!(
            capture.drain_talking(1.0 / 60.0).is_empty(),
            "the closed drain already consumed the sample"
        );
        capture.push_callback_block(&[0.25]);
        assert_eq!(capture.drain_talking(1.0 / 60.0), vec![0.25]);
    }

    #[test]
    fn dropped_frames_count_across_restarts() {
        let mut capture = AudioCapture::stopped_for_test(&VoiceCaptureSettings::default());
        assert!(!capture.is_running(), "a stopped capture is not running");
        assert_eq!(capture.dropped_frames(), 0);
        capture.shared.dropped.store(5, Ordering::Relaxed);
        assert_eq!(capture.dropped_frames(), 5);
        // What `resume` does before swapping the stream: the pre-restart count moves into the
        // base, and the new stream's counter starts from zero on top of it.
        capture.stash_dropped();
        capture.shared.dropped.store(3, Ordering::Relaxed);
        assert_eq!(
            capture.dropped_frames(),
            8,
            "the count survives a stop/resume cycle"
        );
    }

    /// Deep-zai audio-mix finding 13: a `DeviceChanged` stream error (a headset switch) re-opens
    /// the capture on the next drain instead of leaving a live-looking stream that reads silent.
    /// An xrun re-opens nothing; a re-open that finds no device reports the capture closed
    /// (`stream_failed`, so voice chat reads `NoInputDevice`). The re-open itself is faked: no
    /// host microphone is touched.
    #[test]
    fn a_device_change_reopens_the_capture_or_reports_it_closed() {
        let mut capture = AudioCapture::headless(&VoiceCaptureSettings::default());
        let mut opens = 0u32;
        note_stream_error(&capture.shared, cpal::ErrorKind::Xrun);
        let xrun = capture.recover_device_change_with(|_| {
            opens += 1;
            Ok(())
        });
        assert!(xrun.is_none() && opens == 0, "an xrun re-opens nothing");
        assert!(!capture.stream_failed());

        note_stream_error(&capture.shared, cpal::ErrorKind::DeviceChanged);
        assert!(!capture.stream_failed(), "a device change alone is not a failure");
        let changed = capture.recover_device_change_with(|_| {
            opens += 1;
            Ok(())
        });
        assert_eq!(changed, Some(Ok(())), "a device change re-opens the capture");
        assert_eq!(opens, 1);
        let again = capture.recover_device_change_with(|_| {
            opens += 1;
            Ok(())
        });
        assert!(again.is_none() && opens == 1, "one change re-opens once");

        note_stream_error(&capture.shared, cpal::ErrorKind::DeviceChanged);
        let lost = capture
            .recover_device_change_with(|_| Err(AudioError::Capture("no default input device".into())));
        assert!(matches!(lost, Some(Err(AudioError::Capture(_)))), "{lost:?}");
        assert!(
            capture.stream_failed(),
            "a failed re-open reports the capture closed"
        );
        assert!(!capture.is_running());
    }

    /// A device change noted just before a stop (an Android suspend) does not open the
    /// microphone while stopped: the drain only clears it, and `resume` opens afresh.
    #[test]
    fn a_device_change_on_a_stopped_capture_opens_nothing() {
        let mut capture = AudioCapture::stopped_for_test(&VoiceCaptureSettings::default());
        note_stream_error(&capture.shared, cpal::ErrorKind::DeviceChanged);
        assert!(capture.drain(1.0 / 60.0).is_empty());
        assert!(!capture.is_running() && !capture.stream_failed());
        let later = capture.recover_device_change_with(|_| Ok(()));
        assert!(later.is_none(), "the stopped drain cleared the change");
    }

    #[test]
    fn absurd_vad_windows_and_capture_buffers_are_capped() {
        let vad = VoiceActivity::new(VadSettings {
            window: Duration::from_secs(50_000_000_000_000),
            ..VadSettings::default()
        });
        assert_eq!(vad.window.len(), 480_000);
        assert_eq!(capture_buffer_frames(f32::INFINITY, 48_000), 1_440_000);
        assert_eq!(capture_buffer_frames(f32::NAN, 48_000), 1);
        assert_eq!(capture_buffer_frames(-3.0, 48_000), 1);
        assert_eq!(capture_buffer_frames(0.5, 48_000), 24_000);
    }

    #[test]
    fn single_sample_chunk_is_played_and_zero_rate_is_floored() {
        let data = VoiceStreamData::new(0, 0);
        assert_eq!(data.capture_rate, 1.0);
        let mut sound = VoiceStreamSound {
            receiver: data.receiver.clone(),
            stats: Arc::clone(&data.stats),
            placement: Arc::clone(&data.placement),
            prebuffer_frames: 0,
            capture_rate: data.capture_rate,
            queue: VecDeque::new(),
            current: Some((Arc::from(vec![0.5_f32]), 0.0)),
            carry: 0.0,
            primed: true,
            gains: [1.0, 1.0],
            low_pass: 0.0,
        };
        assert_eq!(sound.read_sample(1.0), 0.5);
        assert_eq!(sound.read_sample(1.0), 0.0, "played once, then silence");
        assert!(sound.current.is_none());
    }

    fn stream(chunks: &[&[f32]]) -> VoiceStreamSound {
        let data = VoiceStreamData::new(0, 48_000);
        VoiceStreamSound {
            receiver: data.receiver.clone(),
            stats: Arc::clone(&data.stats),
            placement: Arc::clone(&data.placement),
            prebuffer_frames: 0,
            capture_rate: data.capture_rate,
            queue: chunks
                .iter()
                .map(|c| VoiceChunk {
                    samples: Arc::from(c.to_vec()),
                    captured_at: None,
                })
                .collect(),
            current: None,
            carry: 0.0,
            primed: true,
            gains: [1.0, 1.0],
            low_pass: 0.0,
        }
    }

    /// Audit-gameplay H2 (ADR-1809): no sample is dropped at a chunk edge.
    #[test]
    fn every_sample_of_every_chunk_plays_at_unit_step() {
        let mut sound = stream(&[&[0.0, 0.0, 1.0], &[2.0], &[3.0, 4.0]]);
        let out: Vec<f32> = (0..7).map(|_| sound.read_sample(1.0)).collect();
        assert_eq!(out, [0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 0.0]);
    }

    #[test]
    fn a_480_sample_packet_lasts_480_output_samples() {
        let packet = vec![0.25_f32; 480];
        let mut sound = stream(&[&packet, &packet, &packet]);
        let played = (0..2_000).take_while(|_| sound.read_sample(1.0) != 0.0).count();
        assert_eq!(played, 3 * 480, "no period is skipped per chunk");
    }

    #[test]
    fn a_fractional_step_interpolates_across_the_chunk_edge() {
        let mut sound = stream(&[&[0.0, 1.0], &[3.0]]);
        let out: Vec<f32> = (0..5).map(|_| sound.read_sample(0.5)).collect();
        assert_eq!(out, [0.0, 0.5, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn every_pushed_chunk_is_queued_or_counted_as_dropped() {
        let data = VoiceStreamData::new(0, 48_000);
        let handle = VoiceStreamHandle {
            sender: data.sender.clone(),
            stats: Arc::clone(&data.stats),
            placement: Arc::clone(&data.placement),
        };
        let capacity = data.sender.capacity().unwrap_or(0);
        for _ in 0..capacity + 1 {
            handle.push(Arc::from(vec![0.1_f32]), None);
        }
        let stats = handle.stats();
        assert_eq!(
            stats.chunks_pushed,
            capacity as u64 + 1,
            "every hand-off is counted"
        );
        assert_eq!(stats.chunks_dropped, 1, "the refused one is also dropped");
    }

    // Spec 16 § 3b: 3D positioning for the baseline VoiceStream. The placement reaches the sound
    // through the handle; placed hard right at half gain the right channel carries the sqrt-pan
    // law times the distance gain and the left channel is silent.
    //
    // kira documents `dt` as the time between frames (here 1/48000), so each output sample
    // consumes `dt * capture_rate` = 1 input sample; the constant input makes the step moot.
    #[test]
    fn a_spatial_stream_pans_and_attenuates() {
        use crate::voice_chat::VoicePlacement;
        let data = VoiceStreamData::new(0, 48_000);
        let handle = data.early_handle();
        let mut sound = stream(&[&[0.5_f32; 4096]]);
        sound.placement = Arc::clone(&data.placement);
        handle.set_placement(VoicePlacement::Spatial {
            gain: 0.5,
            pan: 1.0,
            cutoff_hz: None,
        });
        let mut out = vec![Frame::ZERO; 8];
        // The first batch ramps from the flat default toward hard right...
        sound.render(&mut out, 1.0 / 48_000.0);
        assert!(
            out[0].left > 0.0 && out[0].right > out[0].left,
            "the ramp moves toward hard right: {:?}",
            out[0]
        );
        // ...the second batch holds the target exactly (deterministic gains to test).
        sound.render(&mut out, 1.0 / 48_000.0);
        let expected = 0.5 * std::f32::consts::SQRT_2 * 0.5;
        assert!(
            (out[7].right - expected).abs() < 1e-4,
            "right gain {expected}, got {:?}",
            out[7]
        );
        assert!(out[7].left.abs() < 1e-4, "left silent: {:?}", out[7]);
    }

    // Spec 16 § 3b: the default placement stays flat and centred (today's behavior).
    #[test]
    fn a_center_stream_stays_flat() {
        let data = VoiceStreamData::new(0, 48_000);
        let mut sound = stream(&[&[0.25_f32; 4096]]);
        sound.placement = Arc::clone(&data.placement);
        let mut out = vec![Frame::ZERO; 8];
        sound.render(&mut out, 1.0 / 48_000.0);
        sound.render(&mut out, 1.0 / 48_000.0);
        assert_eq!(out[7].left, 0.25);
        assert_eq!(out[7].right, 0.25);
    }

    // Spec 16 § 3b: air absorption low-passes a spatial stream. kira's `dt` is the time between
    // frames (1/48000 here), so the resampler step is 1 and a +1/-1 alternating input renders as
    // a Nyquist-rate alternating signal; a 1 kHz one-pole stops most of it (steady-state
    // amplitude a/(2-a) ≈ 0.065) while passing DC at full gain.
    #[test]
    fn air_absorption_low_passes_a_spatial_stream() {
        use crate::voice_chat::VoicePlacement;
        let nyquist: Vec<f32> = [1.0_f32, -1.0_f32].into_iter().cycle().take(4096).collect();

        let data = VoiceStreamData::new(0, 48_000);
        let handle = data.early_handle();
        let mut filtered = stream(&[&nyquist]);
        filtered.placement = Arc::clone(&data.placement);
        handle.set_placement(VoicePlacement::Spatial {
            gain: 1.0,
            pan: 0.0,
            cutoff_hz: Some(1_000.0),
        });
        let mut out = vec![Frame::ZERO; 8];
        let mut peak = 0.0f32;
        for batch in 0..10 {
            filtered.render(&mut out, 1.0 / 48_000.0);
            if batch == 9 {
                peak = out
                    .iter()
                    .fold(0.0f32, |m, f| m.max(f.left.abs()).max(f.right.abs()));
            }
        }
        assert!(
            (0.02..0.15).contains(&peak),
            "Nyquist mostly stopped (steady-state ≈ 0.065): {peak}"
        );

        // DC passes the filter at gain 1.
        let mut dc = stream(&[&[0.5_f32; 4096]]);
        dc.placement = Arc::clone(&data.placement);
        handle.set_placement(VoicePlacement::Spatial {
            gain: 1.0,
            pan: 0.0,
            cutoff_hz: Some(1_000.0),
        });
        let mut out = vec![Frame::ZERO; 8];
        for _ in 0..10 {
            dc.render(&mut out, 1.0 / 48_000.0);
        }
        assert!((out[7].left - 0.5).abs() < 1e-4, "{:?}", out[7]);
        assert!((out[7].right - 0.5).abs() < 1e-4, "{:?}", out[7]);

        // Without a cutoff the same Nyquist signal passes at full amplitude (a separate
        // stream: the placement channel belongs to the data, and the first one's is filtered).
        let dry_data = VoiceStreamData::new(0, 48_000);
        let mut dry = stream(&[&nyquist]);
        dry.placement = Arc::clone(&dry_data.placement);
        let mut out = vec![Frame::ZERO; 8];
        for _ in 0..10 {
            dry.render(&mut out, 1.0 / 48_000.0);
        }
        let peak = out
            .iter()
            .fold(0.0f32, |m, f| m.max(f.left.abs()).max(f.right.abs()));
        assert!(peak > 0.9, "no cutoff: full amplitude, got {peak}");
    }

    // Spec 16 § 3b: the air-absorption cutoff is compared against the stream's sample rate
    // (1/dt, kira documents `dt` as the time between frames), not scaled up by the batch length.
    // kira's default batch is 128 frames at 48 kHz: a 1 kHz sine under a 4 kHz one-pole passes
    // at nearly full amplitude (an ideal one-pole at fc = 4 × f keeps 0.97); with the batch-scaled
    // rate of 6.1 MHz the same cutoff behaves like ~31 Hz and the sine comes out ~30 dB down.
    #[test]
    fn air_absorption_cutoff_uses_one_over_dt_not_batch_scaled_rate() {
        use crate::voice_chat::VoicePlacement;
        let rate = 48_000.0f64;
        let batch = 128;
        let sine: Vec<f32> = (0..batch * 16)
            .map(|i| 0.5 * (std::f32::consts::TAU * 1_000.0 * i as f32 / rate as f32).sin())
            .collect();

        let data = VoiceStreamData::new(0, 48_000);
        let handle = data.early_handle();
        let mut sound = stream(&[&sine]);
        sound.placement = Arc::clone(&data.placement);
        handle.set_placement(VoicePlacement::Spatial {
            gain: 1.0,
            pan: 0.0,
            cutoff_hz: Some(4_000.0),
        });
        let mut out = vec![Frame::ZERO; batch];
        let mut last = vec![Frame::ZERO; batch];
        for _ in 0..12 {
            sound.render(&mut out, 1.0 / rate);
            last.copy_from_slice(&out);
        }
        let rms = (last.iter().map(|f| f.left * f.left).sum::<f32>() / batch as f32).sqrt();
        assert!(
            rms > 0.25,
            "a 1 kHz sine under a 4 kHz cutoff passes near unity: {rms}"
        );

        // The Nyquist guard now bites: a cutoff at or above rate/2 must pass through untouched.
        let high_data = VoiceStreamData::new(0, 48_000);
        let high_handle = high_data.early_handle();
        let mut high = stream(&[&sine]);
        high.placement = Arc::clone(&high_data.placement);
        high_handle.set_placement(VoicePlacement::Spatial {
            gain: 1.0,
            pan: 0.0,
            cutoff_hz: Some(30_000.0),
        });
        let mut out = vec![Frame::ZERO; batch];
        for _ in 0..12 {
            high.render(&mut out, 1.0 / rate);
        }
        let rms = (out.iter().map(|f| f.left * f.left).sum::<f32>() / batch as f32).sqrt();
        assert!(
            (rms - 0.5f32.sqrt() * 0.5).abs() < 0.02,
            "a cutoff above Nyquist is not low-passed: {rms}"
        );
    }

    // Spec 16 § 3b: a VoiceStreamAnchor on an entity places the stream like an emitter at the
    // entity's position.
    #[test]
    fn an_anchor_places_the_stream_like_an_emitter() {
        use crate::spatial::{EmitterFrame, ListenerFrame, spatialize};
        use crate::voice_chat::VoicePlacement;
        use regia_core::math::Vec3;
        let anchor = VoiceStreamAnchor::default();
        let listener = ListenerFrame::looking_at(Vec3::ZERO, Vec3::NEG_Z);
        let position = Vec3::new(4.0, 0.0, 0.0);
        let frame = EmitterFrame {
            position,
            velocity: Vec3::ZERO,
            forward: Vec3::NEG_Z,
        };
        let expected = spatialize(&listener, &frame, &anchor.emitter_params());
        let VoicePlacement::Spatial { gain, pan, cutoff_hz } =
            stream_placement(&anchor, position, &[listener])
        else {
            panic!("with a listener the anchor is spatial");
        };
        assert!((gain - expected.gain).abs() < 1e-6, "{gain} != {}", expected.gain);
        assert!(pan > 0.9, "4 m to the listener's right pans hard right: {pan}");
        assert_eq!(cutoff_hz, expected.cutoff_hz);
        assert_eq!(
            stream_placement(&anchor, position, &[]),
            VoicePlacement::Center,
            "no listener: the stream stays flat, not silent"
        );
        // Past max distance the stream is silent, like an emitter out of range.
        let far = Vec3::new(0.0, 0.0, -100.0);
        let far_placement = stream_placement(&anchor, far, &[listener]);
        let VoicePlacement::Spatial { gain, .. } = far_placement else {
            panic!("with a listener the anchor is spatial");
        };
        assert_eq!(gain, 0.0);
    }

    #[test]
    fn vad_window_follows_the_device_rate() {
        let settings = VadSettings::default(); // 20 ms
        assert_eq!(
            VoiceActivity::with_sample_rate(settings.clone(), 16_000).window_frames(),
            320
        );
        assert_eq!(VoiceActivity::new(settings.clone()).window_frames(), 960);
        let mut vad = VoiceActivity::with_sample_rate(settings, 96_000);
        assert_eq!(vad.window_frames(), 1_920);
        vad.set_sample_rate(44_100);
        assert_eq!(vad.window_frames(), 882);
    }

    #[test]
    fn vad_survives_non_finite_frames() {
        let mut vad = VoiceActivity::new(VadSettings::default());
        let mut frames = vec![0.3; 960];
        frames[10] = f32::NAN;
        frames[20] = f32::INFINITY;
        vad.process(&frames, 0.02);
        assert!(vad.is_active(), "loud speech still activates");
        assert!(vad.level_db().is_finite());
        for bad_dt in [f64::NAN, -1.0] {
            vad.process(&vec![0.0; 960], bad_dt);
            assert!(vad.is_active(), "a bad step ({bad_dt}) adds no quiet time");
            assert!(vad.level_db().is_finite());
        }
    }

    #[test]
    fn vad_hysteresis_and_hangover() {
        let mut vad = VoiceActivity::new(VadSettings {
            on_threshold_db: -40.0,
            off_threshold_db: -50.0,
            hangover: Duration::from_millis(250),
            window: Duration::from_millis(20),
        });
        let block = Duration::from_millis(20);
        // Silence: 48 kHz · 20 ms = 960 frames.
        vad.process(&vec![0.0; 960], block.as_secs_f64());
        assert!(!vad.is_active());

        // Loud speech (0.3 amplitude ≈ -10 dBFS): activates.
        vad.process(&vec![0.3; 960], block.as_secs_f64());
        assert!(vad.is_active());

        // Quiet (≈ -60 dBFS): stays active through the 250 ms hangover (13 blocks of 20 ms),
        // then drops.
        let quiet = vec![0.001; 960];
        for _ in 0..12 {
            vad.process(&quiet, block.as_secs_f64());
            assert!(vad.is_active(), "hangover keeps activity");
        }
        vad.process(&quiet, block.as_secs_f64());
        vad.process(&quiet, block.as_secs_f64());
        assert!(!vad.is_active(), "activity ends after the hangover");

        // Mid-level noise between the thresholds neither triggers nor matters.
        vad.process(&vec![0.005; 960], block.as_secs_f64()); // ≈ -46 dBFS
        assert!(!vad.is_active(), "hysteresis: between thresholds nothing changes");
    }

    #[test]
    fn dbfs_scales() {
        assert!(dbfs(1.0).abs() < 1e-6);
        assert!((dbfs(0.5) + 6.02).abs() < 0.01);
        assert!(dbfs(0.0) < -99.0);
    }

    /// The level meter averages signal **power**: after a loud block ends, the meter falls back
    /// toward the silence it now hears, instead of climbing to a phantom level (the amplitude /
    /// power mix-up this test pins: `sqrt(0.9·rms + 0.1·mono²)` converges to −0.5 dBFS on
    /// silence after any single noise sample).
    #[test]
    fn level_meter_is_a_power_ema() {
        let mut power = 0.0f32;
        // A loud 20 ms block (960 samples at 0.5): the meter reads ≈ 0.5.
        for _ in 0..960 {
            power = meter_step(power, 0.5);
        }
        assert!(
            (power.sqrt() - 0.5).abs() < 0.05,
            "tracks loud input: {}",
            power.sqrt()
        );

        // Then silence: the meter decays back down instead of climbing.
        for _ in 0..960 {
            power = meter_step(power, 0.0);
        }
        assert!(power.sqrt() < 0.01, "decays on silence: {}", power.sqrt());

        // A single tiny noise sample in silence must not climb to a phantom level.
        power = 0.0;
        power = meter_step(power, 3e-5); // ≈ −90 dBFS noise
        for _ in 0..960 {
            power = meter_step(power, 0.0);
        }
        assert!(
            power.sqrt() < 0.01,
            "no phantom level from noise: {}",
            power.sqrt()
        );
    }

    /// ADR-1809: a full playback queue drops its oldest chunk and counts it.
    #[test]
    fn a_full_playback_queue_counts_the_chunk_it_drops() {
        // A 1 s prebuffer keeps the stream silent, so nothing is consumed from the queue.
        let data = VoiceStreamData::new(1_000, 48_000);
        let handle = data.early_handle();
        let (mut sound, _playback) = data.into_sound().expect("never fails");
        let info = kira::info::MockInfoBuilder::new().build();
        let mut out = vec![Frame::ZERO; 16];
        for _ in 0..128 {
            handle.push(vec![0.1_f32].into(), None);
        }
        sound.process(&mut out, 1.0 / 48_000.0, &info);
        assert_eq!(handle.stats().chunks_dropped, 0, "128 chunks fit the queue");
        handle.push(vec![0.1_f32].into(), None);
        sound.process(&mut out, 1.0 / 48_000.0, &info);
        let stats = handle.stats();
        assert_eq!(stats.chunks_pushed, 129);
        assert_eq!(
            stats.chunks_dropped, 1,
            "the oldest queued chunk made room and was counted"
        );
    }

    #[test]
    fn voice_stream_plays_pushed_frames_without_a_manager() {
        let data = VoiceStreamData::new(10, 48_000);
        let handle = data.early_handle();
        let (mut sound, _playback) = data.into_sound().expect("never fails");
        let info = kira::info::MockInfoBuilder::new().build();

        // Silence while the prebuffer fills.
        let mut out = vec![Frame::ZERO; 480];
        sound.process(&mut out, 1.0 / 48_000.0, &info);
        assert!(out.iter().all(|f| f.left == 0.0));

        // Push two chunks of a loud ramp; the prebuffer (10 ms = 480 frames) primes.
        handle.push(vec![0.5; 480].into(), Some(Instant::now()));
        handle.push(vec![0.5; 480].into(), None);
        sound.process(&mut out, 1.0 / 48_000.0, &info);
        assert!(out.iter().any(|f| f.left > 0.4), "voice frames are audible");
        let stats = handle.stats();
        assert!(stats.chunks_pushed == 2);
        assert!(stats.last_latency_ms < 1_000, "latency sane: {stats:?}");
        assert!(!sound.finished());

        // Stopping the handle finishes the sound.
        handle.stop();
        assert!(sound.finished());
    }

    #[test]
    fn resampling_reads_at_the_capture_rate() {
        // 2:1 upsample: output consumes half a capture sample per frame.
        let data = VoiceStreamData::new(0, 24_000);
        let handle = data.early_handle();
        let (mut sound, _) = data.into_sound().expect("never fails");
        let info = kira::info::MockInfoBuilder::new().build();
        let ramp: Arc<[f32]> = (0..48).map(|i| i as f32 / 48.0).collect();
        handle.push(ramp, None);
        let mut out = vec![Frame::ZERO; 24];
        sound.process(&mut out, 1.0 / 48_000.0, &info);
        // First frames interpolate near zero, later frames rise.
        assert!(out[0].left < out[23].left, "ramp rises across the render");
        assert!(out[23].left > 0.0);
    }
}
