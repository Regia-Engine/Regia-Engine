// SPDX-License-Identifier: MPL-2.0
//! Local transmit state machine, talk gating and frame-based activity detection
//! (`docs/specs/59-voice-chat.md` §§ 2.2, 2.6). Time is counted in 20 ms frames, never read from
//! a clock.

use super::error::VoiceChatError;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Frames in the default 200 ms hangover.
pub const HANGOVER_FRAMES: u32 = 10;
/// Frames of push-to-talk release grace (150 ms rounded up to whole frames).
pub const PTT_GRACE_FRAMES: u32 = 8;
/// Frames between device retries after a removal (2 s).
pub const RETRY_FRAMES: u32 = 100;
/// Frames in the 5 s adaptive noise-floor window.
pub const NOISE_FLOOR_FRAMES: u32 = 250;
/// Margin above the tracked noise floor for the speech-on threshold.
pub const ADAPTIVE_ON_MARGIN_DB: f32 = 10.0;
/// Margin above the tracked noise floor for the speech-off threshold.
pub const ADAPTIVE_OFF_MARGIN_DB: f32 = 5.0;

/// How the player decides to transmit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VoiceMode {
    /// Always transmit.
    Open,
    /// Transmit while speech is detected.
    Vad,
    /// Transmit while the `voice_chat_talk` action is held. The default everywhere (lowest
    /// accidental-leak risk).
    #[default]
    PushToTalk,
    /// The action toggles transmitting.
    Toggle,
}

impl VoiceMode {
    /// Lowercase name for the agent protocol.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Vad => "vad",
            Self::PushToTalk => "push_to_talk",
            Self::Toggle => "toggle",
        }
    }
}

/// State of the local transmitter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxState {
    /// No consent or no device.
    Off,
    /// Device open, not transmitting.
    Idle,
    /// Sending frames.
    Transmitting,
    /// Activity ended, tail still sent.
    Hangover,
    /// Self mute: nothing is encoded.
    Muted,
    /// Failed; carries the agent code.
    Error(&'static str),
}

impl TxState {
    /// Lowercase name for the agent protocol.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Idle => "idle",
            Self::Transmitting => "transmitting",
            Self::Hangover => "hangover",
            Self::Muted => "muted",
            Self::Error(_) => "error",
        }
    }
}

/// What the transmitter wants done for one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxFrame {
    /// Encode and send this frame.
    pub transmit: bool,
    /// The packet is the last of the talk spurt.
    pub end_of_talk: bool,
}

/// The transmit state machine of spec § 2.2.
#[derive(Clone, Debug)]
pub struct TransmitMachine {
    state: TxState,
    hangover_frames: u32,
    hangover_left: u32,
    retry_left: u32,
    was_live: bool,
    return_muted: bool,
}

impl Default for TransmitMachine {
    fn default() -> Self {
        Self::new(HANGOVER_FRAMES)
    }
}

impl TransmitMachine {
    /// A machine in `Off` with the given hangover length.
    #[must_use]
    pub fn new(hangover_frames: u32) -> Self {
        Self {
            state: TxState::Off,
            hangover_frames,
            hangover_left: 0,
            retry_left: 0,
            was_live: false,
            return_muted: false,
        }
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> &TxState {
        &self.state
    }

    fn live(&self) -> bool {
        matches!(
            self.state,
            TxState::Idle | TxState::Transmitting | TxState::Hangover
        )
    }

    /// Consent was granted and the device open attempt finished with `result`. Only `Off` (and
    /// `Error`, on an explicit retry) leaves for `Idle`/`Error`.
    pub fn device_opened(&mut self, result: Result<(), VoiceChatError>) {
        if !matches!(self.state, TxState::Off | TxState::Error(_)) {
            return;
        }
        match result {
            Ok(()) => self.state = TxState::Idle,
            Err(e) => self.state = TxState::Error(e.code()),
        }
    }

    /// The device went away. Returns whether an `end_of_talk` packet is owed.
    pub fn device_removed(&mut self) -> bool {
        if !self.live() && self.state != TxState::Muted {
            return false;
        }
        let owed = matches!(self.state, TxState::Transmitting | TxState::Hangover);
        self.was_live = true;
        self.return_muted = self.state == TxState::Muted;
        self.state = TxState::Error("no_input_device");
        self.retry_left = RETRY_FRAMES;
        owed
    }

    /// The microphone was closed on purpose (consent withdrawn, app paused). Returns whether an
    /// `end_of_talk` packet is owed.
    pub fn device_closed(&mut self) -> bool {
        let owed = matches!(self.state, TxState::Transmitting | TxState::Hangover);
        self.state = TxState::Off;
        self.was_live = false;
        owed
    }

    /// Self mute on or off. Returns whether an `end_of_talk` packet is owed.
    pub fn set_muted(&mut self, muted: bool) -> bool {
        if muted {
            let owed = matches!(self.state, TxState::Transmitting | TxState::Hangover);
            if self.live() {
                self.state = TxState::Muted;
            }
            owed
        } else {
            self.return_muted = false;
            if self.state == TxState::Muted {
                self.state = TxState::Idle;
            }
            false
        }
    }

    /// Counts a frame while in `Error(no_input_device)`; returns to `Idle` once the retry delay
    /// elapsed and a device is present.
    pub fn retry(&mut self, device_present: bool) {
        if self.state != TxState::Error("no_input_device") || !self.was_live {
            return;
        }
        self.retry_left = self.retry_left.saturating_sub(1);
        if self.retry_left == 0 && device_present {
            self.state = if self.return_muted {
                TxState::Muted
            } else {
                TxState::Idle
            };
        }
    }

    /// Advances one frame. `wants_talk` is the talk gate's decision for this frame.
    pub fn step(&mut self, wants_talk: bool) -> TxFrame {
        match self.state {
            TxState::Idle if wants_talk => {
                self.state = TxState::Transmitting;
                TxFrame {
                    transmit: true,
                    end_of_talk: false,
                }
            }
            TxState::Transmitting if wants_talk => TxFrame {
                transmit: true,
                end_of_talk: false,
            },
            TxState::Transmitting => {
                self.hangover_left = self.hangover_frames;
                self.state = TxState::Hangover;
                self.tail_frame()
            }
            TxState::Hangover if wants_talk => {
                self.state = TxState::Transmitting;
                TxFrame {
                    transmit: true,
                    end_of_talk: false,
                }
            }
            TxState::Hangover => self.tail_frame(),
            _ => TxFrame::default(),
        }
    }

    fn tail_frame(&mut self) -> TxFrame {
        if self.hangover_left <= 1 {
            self.hangover_left = 0;
            self.state = TxState::Idle;
            TxFrame {
                transmit: true,
                end_of_talk: true,
            }
        } else {
            self.hangover_left -= 1;
            TxFrame {
                transmit: true,
                end_of_talk: false,
            }
        }
    }
}

/// Settings of the frame-based activity detector.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameVadSettings {
    /// RMS level (dBFS) that marks speech onset.
    pub on_db: f32,
    /// RMS level (dBFS) below which speech ends.
    pub off_db: f32,
    /// Track the noise floor and raise both thresholds above it.
    pub adaptive: bool,
}

impl Default for FrameVadSettings {
    fn default() -> Self {
        Self {
            on_db: -40.0,
            off_db: -50.0,
            adaptive: false,
        }
    }
}

/// Short-term RMS (dBFS) of a frame; `-120` for silence or an empty frame.
#[must_use]
pub fn frame_rms_db(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return -120.0;
    }
    let mean = frame.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>() / frame.len() as f64;
    if mean <= 1e-12 {
        -120.0
    } else {
        (10.0 * mean.log10()) as f32
    }
}

/// Energy detector with hysteresis, one decision per 20 ms frame (the active-speech idea of
/// ITU-T P.56). The adaptive noise floor is the minimum frame level over the last
/// [`NOISE_FLOOR_FRAMES`] frames (5 s), so pauses between words set it and it follows a changing
/// room within the window.
#[derive(Clone, Debug)]
pub struct FrameVad {
    settings: FrameVadSettings,
    active: bool,
    history: VecDeque<f32>,
}

impl FrameVad {
    /// A detector with the given settings.
    #[must_use]
    pub fn new(settings: FrameVadSettings) -> Self {
        Self {
            settings,
            active: false,
            history: VecDeque::new(),
        }
    }

    fn floor_db(&self) -> f32 {
        self.history.iter().copied().fold(f32::INFINITY, f32::min)
    }

    /// The speech-on threshold currently in force.
    #[must_use]
    pub fn on_threshold_db(&self) -> f32 {
        if self.settings.adaptive && !self.history.is_empty() {
            self.settings.on_db.max(self.floor_db() + ADAPTIVE_ON_MARGIN_DB)
        } else {
            self.settings.on_db
        }
    }

    /// The speech-off threshold currently in force.
    #[must_use]
    pub fn off_threshold_db(&self) -> f32 {
        if self.settings.adaptive && !self.history.is_empty() {
            self.settings.off_db.max(self.floor_db() + ADAPTIVE_OFF_MARGIN_DB)
        } else {
            self.settings.off_db
        }
    }

    /// Whether speech is currently detected.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Processes one frame and returns whether speech is active.
    pub fn process(&mut self, frame: &[f32]) -> bool {
        let db = frame_rms_db(frame);
        if self.settings.adaptive {
            if self.history.len() >= NOISE_FLOOR_FRAMES as usize {
                self.history.pop_front();
            }
            self.history.push_back(db);
        }
        if self.active {
            if db < self.off_threshold_db() {
                self.active = false;
            }
        } else if db >= self.on_threshold_db() {
            self.active = true;
        }
        self.active
    }
}

/// Turns the mode, the activity detector and the talk action into one decision per frame.
#[derive(Clone, Debug)]
pub struct TalkGate {
    mode: VoiceMode,
    vad: FrameVad,
    grace_left: u32,
    toggled_on: bool,
    prev_pressed: bool,
}

impl TalkGate {
    /// A gate for `mode`.
    #[must_use]
    pub fn new(mode: VoiceMode, vad: FrameVadSettings) -> Self {
        Self {
            mode,
            vad: FrameVad::new(vad),
            grace_left: 0,
            toggled_on: false,
            prev_pressed: false,
        }
    }

    /// The detector (for threshold readouts).
    #[must_use]
    pub fn vad(&self) -> &FrameVad {
        &self.vad
    }

    /// Switches mode and clears latches.
    pub fn set_mode(&mut self, mode: VoiceMode) {
        self.mode = mode;
        self.grace_left = 0;
        self.toggled_on = false;
    }

    /// Decides for one frame. `pressed` is the `voice_chat_talk` action state.
    pub fn wants_talk(&mut self, frame: &[f32], pressed: bool) -> bool {
        let active = self.vad.process(frame);
        let rising = pressed && !self.prev_pressed;
        self.prev_pressed = pressed;
        match self.mode {
            VoiceMode::Open => true,
            VoiceMode::Vad => active,
            VoiceMode::PushToTalk => {
                if pressed {
                    self.grace_left = PTT_GRACE_FRAMES;
                    true
                } else if self.grace_left > 0 {
                    self.grace_left -= 1;
                    true
                } else {
                    false
                }
            }
            VoiceMode::Toggle => {
                if rising {
                    self.toggled_on = !self.toggled_on;
                }
                self.toggled_on
            }
        }
    }
}
