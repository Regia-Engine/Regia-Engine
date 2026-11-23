// SPDX-License-Identifier: MPL-2.0
//! Microphone input for a voice chat session (`docs/specs/59-voice-chat.md` § 2.5).
//!
//! [`InputDeviceOpener`] is the seam: [`CpalInputOpener`] opens the real device through the capture
//! baseline ([`crate::AudioCapture`]), asking the platform for the microphone permission first
//! (Android `RECORD_AUDIO`), and [`FakeMicrophone`] is the scripted stand-in tests and the agent
//! protocol use, so nothing here needs a sound device.

use super::error::VoiceChatError;
use super::source::TestSource;
use crate::error::AudioError;
use crate::voice::{AudioCapture, VoiceCaptureSettings};
use regia_platform::{MicrophonePermission, microphone_permission, request_microphone_permission};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

/// What opening an input device returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDevice {
    /// Device name.
    pub name: String,
    /// Native sample rate.
    pub rate: u32,
    /// Interleaved channel count.
    pub channels: usize,
}

/// The microphone permission as an opener reports it (spec § 2.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicPermission {
    /// Access is allowed; the device may be opened.
    Granted,
    /// Nobody asked yet: [`InputDeviceOpener::request_permission`] shows the prompt.
    NotAsked,
    /// The prompt is on screen.
    Pending,
    /// Access was refused. `can_prompt` is false after "don't ask again": only the system settings
    /// can change it.
    Denied {
        /// Asking again can still show a prompt.
        can_prompt: bool,
    },
}

/// Opens the microphone and reads it. The session only calls it after consent; tests pass a mock
/// that counts opens.
pub trait InputDeviceOpener {
    /// Opens the named device (`None` = system default).
    ///
    /// # Errors
    /// `permission_denied`, `no_input_device`, `device_busy` or `unsupported_format`.
    fn open(&mut self, name: Option<&str>) -> Result<InputDevice, VoiceChatError>;

    /// The current permission state. Polled every voice frame while a prompt is on screen.
    fn permission(&mut self) -> MicPermission {
        MicPermission::Granted
    }

    /// Shows the system prompt. Only ever called from an explicit enable action.
    fn request_permission(&mut self) {}

    /// Appends the interleaved samples captured since the last call.
    ///
    /// # Errors
    /// [`VoiceChatError::NoInputDevice`] when the device went away.
    fn read(&mut self, _out: &mut Vec<f32>) -> Result<(), VoiceChatError> {
        Ok(())
    }

    /// Closes the stream and releases the microphone. Safe when nothing is open.
    fn close(&mut self) {}

    /// Samples the capture ring dropped so far (the game thread fell behind).
    fn dropped_samples(&self) -> u64 {
        0
    }
}

/// Maps the capture baseline's error text to an agent code.
#[must_use]
pub fn classify_capture_error(error: &AudioError) -> VoiceChatError {
    let text = error.to_string().to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| text.contains(w));
    if has(&["permission", "denied", "not allowed", "unauthorized", "privacy"]) {
        VoiceChatError::PermissionDenied
    } else if has(&["busy", "in use", "exclusive"]) {
        VoiceChatError::DeviceBusy
    } else if has(&["format", "sample rate", "not supported", "unsupported"]) {
        VoiceChatError::UnsupportedFormat(text)
    } else {
        VoiceChatError::NoInputDevice
    }
}

/// The real microphone: the platform's permission flow, then a cpal input stream through
/// [`AudioCapture`]. Closing drops the stream, so the OS indicator goes off.
#[derive(Default)]
pub struct CpalInputOpener {
    settings: VoiceCaptureSettings,
    capture: Option<AudioCapture>,
    /// Dropped samples of streams already closed.
    dropped_before: u64,
}

impl CpalInputOpener {
    /// An opener that has not touched any device yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An opener with capture settings of its own (ring size, requested rate).
    #[must_use]
    pub fn with_settings(settings: VoiceCaptureSettings) -> Self {
        Self {
            settings,
            ..Self::default()
        }
    }
}

impl InputDeviceOpener for CpalInputOpener {
    fn open(&mut self, name: Option<&str>) -> Result<InputDevice, VoiceChatError> {
        self.close();
        let mut settings = self.settings.clone();
        settings.device = name.map(str::to_string);
        let capture = match AudioCapture::start(&settings) {
            Ok(capture) => capture,
            // A saved device that is gone falls back to the default and logs once (spec § 2.5).
            Err(error) if name.is_some() => {
                tracing::warn!(target: "regia_audio", "voice chat input {name:?} failed ({error}); using the default");
                settings.device = None;
                AudioCapture::start(&settings).map_err(|e| classify_capture_error(&e))?
            }
            Err(error) => return Err(classify_capture_error(&error)),
        };
        // The baseline stream already mixes down to mono.
        let device = InputDevice {
            name: capture.device().to_string(),
            rate: capture.sample_rate(),
            channels: 1,
        };
        self.capture = Some(capture);
        Ok(device)
    }

    fn permission(&mut self) -> MicPermission {
        match microphone_permission() {
            MicrophonePermission::Granted => MicPermission::Granted,
            MicrophonePermission::NotAsked => MicPermission::NotAsked,
            MicrophonePermission::Pending => MicPermission::Pending,
            MicrophonePermission::Denied { can_prompt } => MicPermission::Denied { can_prompt },
        }
    }

    fn request_permission(&mut self) {
        request_microphone_permission();
    }

    fn read(&mut self, out: &mut Vec<f32>) -> Result<(), VoiceChatError> {
        let Some(capture) = &mut self.capture else {
            return Ok(());
        };
        if capture.stream_failed() {
            return Err(VoiceChatError::NoInputDevice);
        }
        capture.drain_into(out);
        Ok(())
    }

    fn close(&mut self) {
        if let Some(mut capture) = self.capture.take() {
            self.dropped_before += capture.dropped_frames();
            capture.stop();
        }
    }

    fn dropped_samples(&self) -> u64 {
        self.dropped_before + self.capture.as_ref().map_or(0, AudioCapture::dropped_frames)
    }
}

#[derive(Debug)]
struct FakeMicState {
    rate: u32,
    channels: usize,
    name: String,
    queued: VecDeque<f32>,
    open: bool,
    present: bool,
    open_error: Option<VoiceChatError>,
    permission: MicPermission,
    answer: Option<MicPermission>,
    opens: u32,
    closes: u32,
    requests: u32,
    tone: Option<TestSource>,
}

/// A scripted microphone: tests push samples, change the permission answer and pull the device.
/// Cloning gives another handle on the same microphone, so a test keeps one after handing the
/// other to a session.
#[derive(Clone, Debug)]
pub struct FakeMicrophone(Arc<Mutex<FakeMicState>>);

impl FakeMicrophone {
    /// A present, permitted microphone at `rate` Hz with `channels` interleaved channels.
    #[must_use]
    pub fn new(rate: u32, channels: usize) -> Self {
        Self(Arc::new(Mutex::new(FakeMicState {
            rate,
            channels,
            name: "fake microphone".to_string(),
            queued: VecDeque::new(),
            open: false,
            present: true,
            open_error: None,
            permission: MicPermission::Granted,
            answer: None,
            opens: 0,
            closes: 0,
            requests: 0,
            tone: None,
        })))
    }

    fn state(&self) -> MutexGuard<'_, FakeMicState> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queues interleaved samples for the next read (ignored while the stream is closed, as a real
    /// device would not record them).
    pub fn push(&self, samples: &[f32]) {
        let mut s = self.state();
        if s.open {
            s.queued.extend(samples.iter().copied());
        }
    }

    /// Makes every read deliver 20 ms of `source` on all channels, so the microphone speaks without
    /// a test pushing samples.
    pub fn set_tone(&self, source: Option<TestSource>) {
        self.state().tone = source;
    }

    /// Sets the permission state.
    pub fn set_permission(&self, permission: MicPermission) {
        self.state().permission = permission;
    }

    /// The state a pending prompt resolves to: [`request_permission`] moves to `Pending`, and
    /// [`resolve_prompt`](Self::resolve_prompt) to this answer.
    ///
    /// [`request_permission`]: InputDeviceOpener::request_permission
    pub fn answer_prompt_with(&self, answer: MicPermission) {
        self.state().answer = Some(answer);
    }

    /// The user answered the prompt.
    pub fn resolve_prompt(&self) {
        let mut s = self.state();
        if let Some(answer) = s.answer.take() {
            s.permission = answer;
        }
    }

    /// Makes the next opens fail with `error` (`None` clears it).
    pub fn set_open_error(&self, error: Option<VoiceChatError>) {
        self.state().open_error = error;
    }

    /// Unplugs (`false`) or plugs in (`true`) the device.
    pub fn set_present(&self, present: bool) {
        let mut s = self.state();
        s.present = present;
        if !present {
            s.queued.clear();
        }
    }

    /// Successful and failed open calls so far.
    #[must_use]
    pub fn opens(&self) -> u32 {
        self.state().opens
    }

    /// Close calls that closed an open stream.
    #[must_use]
    pub fn closes(&self) -> u32 {
        self.state().closes
    }

    /// Prompts shown so far.
    #[must_use]
    pub fn requests(&self) -> u32 {
        self.state().requests
    }

    /// Whether the stream is open now.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.state().open
    }
}

impl InputDeviceOpener for FakeMicrophone {
    fn open(&mut self, name: Option<&str>) -> Result<InputDevice, VoiceChatError> {
        let mut s = self.state();
        s.opens += 1;
        if let Some(error) = s.open_error.clone() {
            return Err(error);
        }
        if !s.present {
            return Err(VoiceChatError::NoInputDevice);
        }
        s.open = true;
        s.queued.clear();
        Ok(InputDevice {
            name: name.map_or_else(|| s.name.clone(), str::to_string),
            rate: s.rate,
            channels: s.channels,
        })
    }

    fn permission(&mut self) -> MicPermission {
        self.state().permission
    }

    fn request_permission(&mut self) {
        let mut s = self.state();
        s.requests += 1;
        s.permission = MicPermission::Pending;
    }

    fn read(&mut self, out: &mut Vec<f32>) -> Result<(), VoiceChatError> {
        let mut s = self.state();
        if s.open && !s.present {
            return Err(VoiceChatError::NoInputDevice);
        }
        let (rate, channels) = (s.rate, s.channels);
        out.extend(s.queued.drain(..));
        if s.open
            && let Some(tone) = s.tone.as_mut()
        {
            let mut mono = vec![0.0f32; (rate / 50) as usize];
            tone.fill(&mut mono, rate);
            out.extend(mono.iter().flat_map(|v| std::iter::repeat_n(*v, channels)));
        }
        Ok(())
    }

    fn close(&mut self) {
        let mut s = self.state();
        if s.open {
            s.open = false;
            s.closes += 1;
        }
        s.queued.clear();
    }
}
