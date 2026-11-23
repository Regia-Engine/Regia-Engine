// SPDX-License-Identifier: MPL-2.0
//! The local participant's session: consent, transmit path, receive path, channels, moderation
//! and statistics (`docs/specs/59-voice-chat.md` §§ 2.2-2.15). One call to
//! [`VoiceChatSession::tick`] is one 20 ms voice frame; nothing reads a clock.

use super::VoiceChatTransport;
use super::capture::CaptureConverter;
use super::codec::{BuiltinCodecs, SharedCodecProvider, VoiceCodec};
use super::error::VoiceChatError;
use super::mic::{InputDevice, InputDeviceOpener, MicPermission};
use super::moderation::{ReportRing, VoiceReport, VoiceReportSink};
use super::packet::{
    FRAME_TIMESTAMP_STEP, MAX_PAYLOAD, MAX_PCM_PAYLOAD, VoiceCodecId, VoiceFlags, VoicePacket, VoiceQuality,
    write_multi,
};
use super::processing::{EchoReference, ProcessingChain, ProcessingStats, mix_into};
use super::routing::{Candidate, RANK_STAY_IN_MARGIN_DB, rank_speakers_sticky_into};
use super::settings::{
    LOOPBACK_MAX_FRAMES, LOOPBACK_SENDER, ProcessingSettings, VoiceChannel, VoiceChannelId,
    VoiceChatSettings, VoiceChatUserSettings,
};
use super::simulator::{SimulatorSettings, VoiceChatSimulator};
use super::source::TestSource;
use super::speaker::{SpeakerState, VoiceSpeaker};
use super::transmit::{RETRY_FRAMES, TalkGate, TransmitMachine, TxState, VoiceMode, frame_rms_db};
use regia_ecs::prelude::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

/// Frames between re-evaluations of the audible speaker set (spec § 2.4).
const RANK_INTERVAL_FRAMES: u64 = 10;

/// Whole frames of capture backlog kept; older ones are dropped and counted (a stalled game must
/// not send seconds of old audio).
const MAX_CAPTURE_BACKLOG_FRAMES: usize = 5;
/// Frames between adaptive bit rate steps (2 s, spec § 2.8).
const BITRATE_STEP_FRAMES: u64 = 100;
/// Loss in percent above which the bit rate steps down.
const BITRATE_DOWN_LOSS: f32 = 5.0;
/// Loss in percent below which the bit rate steps back up.
const BITRATE_UP_LOSS: f32 = 2.0;
/// Measured loss in percent above which `redundancy` sends a spare frame (spec § 2.8).
const REDUNDANCY_LOSS: f32 = 10.0;
/// Frames between comfort packets while an open microphone is silent (400 ms, spec § 2.2).
const COMFORT_INTERVAL_FRAMES: u32 = 20;
/// Control messages kept for a transport that has not drained them.
const MAX_PENDING_CONTROL: usize = 256;
/// Frames kept for the audio output between two drains (one second).
const MAX_PENDING_OUTPUT: usize = 50 * 16;
/// Most remote speakers a session decodes and mixes. Sender slots come from the server (a listen
/// server is another player's machine): each new slot was a decoder and a jitter buffer, rendered
/// every frame, without a limit (ADR-5870). A packet from a new sender past it is dropped and
/// counted in [`SessionStats::overflow_drops`].
pub const MAX_REMOTE_SPEAKERS: usize = 256;
/// Most sender slots bound to an account ([`VoiceChatSession::bind_sender`]); a new binding past
/// it is ignored.
pub const MAX_BOUND_SENDERS: usize = 1024;
/// Voice frames between two `SpeakerActivity` events of one sender: the 10 Hz debounce of spec 59
/// § 2.14 (100 ms).
pub const ACTIVITY_DEBOUNCE_FRAMES: u64 = 5;
/// `DecodedFrame` events kept for a game that does not drain [`VoiceChatSession::take_events`];
/// later frames are not queued until it does (about 5 s of one speaker).
pub const MAX_PENDING_DECODED_FRAMES: usize = 256;

/// Where the microphone path stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MicPhase {
    /// Closed.
    Off,
    /// The system prompt is on screen.
    AwaitingPermission,
    /// Streaming.
    Open,
    /// The device went away; retried every [`RETRY_FRAMES`].
    Lost,
    /// Closed by [`VoiceChatSession::suspend_microphone`]; reopens on resume.
    Suspended,
}

impl MicPhase {
    fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::AwaitingPermission => "awaiting_permission",
            Self::Open => "open",
            Self::Lost => "lost",
            Self::Suspended => "suspended",
        }
    }
}

/// A membership or moderation change the server must hear about (spec § 2.10: a small reliable
/// message, never a voice packet). The session queues them; a network transport drains them with
/// [`VoiceChatSession::take_control`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoiceControl {
    /// Joined a channel.
    Join(VoiceChannelId),
    /// Left a channel.
    Leave(VoiceChannelId),
    /// Stop or resume forwarding a sender to this listener (`mute_hint`).
    MuteHint {
        /// Sender slot.
        sender: u16,
        /// Muted now.
        muted: bool,
    },
    /// Block or unblock an account (mutual at the server).
    Block {
        /// Account tag.
        account: String,
        /// Blocked now.
        blocked: bool,
    },
}

/// What a transport measured on its link, fed to the adaptive bit rate (spec § 2.8).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VoiceLinkQuality {
    /// Packet loss in percent.
    pub loss_pct: f32,
    /// Round trip time in milliseconds.
    pub rtt_ms: f32,
}

/// Events for the game (spec §§ 3, 2.14).
#[derive(Clone, Debug, PartialEq)]
pub enum VoiceChatEvent {
    /// The local transmit state changed.
    StateChanged {
        /// New state name.
        state: &'static str,
    },
    /// A talk spurt started or ended.
    SpeakerActivity {
        /// Sender slot.
        sender: u16,
        /// Channel of the last packet.
        channel: u16,
        /// Speaking now.
        active: bool,
        /// Level 0..1 (linear RMS of the last frame).
        level: f32,
    },
    /// The game's UI asked to report a speaker.
    ReportRequested {
        /// Sender slot.
        sender: u16,
        /// Channel.
        channel: u16,
        /// Seconds of audio handed over.
        seconds: u32,
    },
    /// One decoded 20 ms frame of a heard speaker (mono, before the output gain), sent only while
    /// [`VoiceChatSession::set_emit_decoded_frames`] is on (spec § 2.14), for a game's own
    /// captioning or visualization.
    DecodedFrame {
        /// Sender slot.
        sender: u16,
        /// Channel of the last packet.
        channel: u16,
        /// Sample rate of the frame.
        rate: u32,
        /// The decoded samples.
        samples: Arc<[f32]>,
    },
    /// The result of a permission request.
    PermissionResult {
        /// Whether access was granted.
        granted: bool,
    },
    /// An error worth surfacing.
    Error(VoiceChatError),
}

/// Counters of the session (spec § 10 and 5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionStats {
    /// Frames the local transmitter encoded.
    pub frames_encoded: u64,
    /// Packets handed to the transport.
    pub packets_sent: u64,
    /// Bytes handed to the transport (voice header plus payload).
    pub bytes_sent: u64,
    /// Packets accepted from the transport.
    pub packets_received: u64,
    /// Bytes received.
    pub bytes_received: u64,
    /// Packets that did not parse.
    pub bad_packets: u64,
    /// Packets for a channel the listener has not joined.
    pub wrong_channel: u64,
    /// Packets dropped because the sender is muted or blocked locally.
    pub muted_drops: u64,
    /// Packets dropped because the listener is deafened.
    pub deafened_drops: u64,
    /// Packets from a sender that lost the audible-set ranking, or from a new sender past
    /// [`MAX_REMOTE_SPEAKERS`].
    pub overflow_drops: u64,
    /// Capture frames dropped because the backlog grew past 100 ms.
    pub capture_overruns: u64,
    /// Encoder failures (the frame is not sent).
    pub encode_errors: u64,
    /// Encoded frames over the codec's payload cap (never sent; a bug in the codec's rate limit).
    pub oversize_frames: u64,
}

/// How often one error code happened to the running session (spec § 5): the codes a stream meets
/// without an agent call to refuse (`bad_packet`, `codec_error`, `server_muted`, device codes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamErrorRecord {
    /// The lowercase agent code (one of [`super::ERROR_CODES`]).
    pub code: &'static str,
    /// Times it happened since the session started or the stats were reset.
    pub count: u64,
    /// The voice frame it last happened on.
    pub last_frame: u64,
}

/// The debounce state of one sender's `SpeakerActivity` events.
#[derive(Clone, Copy, Debug, Default)]
struct ActivityDebounce {
    /// The state the last event reported (`false` before any).
    reported: bool,
    /// The frame of the last event.
    reported_at: Option<u64>,
    /// The newest edge not reported yet: `(active, level_db)`.
    pending: Option<(bool, f32)>,
}

/// One remote speaker as shown by `voice_chat.status`.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerStatus {
    /// Sender slot.
    pub sender: u16,
    /// State name.
    pub state: &'static str,
    /// Locally muted.
    pub muted: bool,
    /// Blocked.
    pub blocked: bool,
    /// Level in dBFS.
    pub level_db: f32,
}

/// Snapshot of the session for the agent protocol.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceChatStatus {
    /// Transmit state name.
    pub state: &'static str,
    /// Agent code of the error state, if any.
    pub error: Option<&'static str>,
    /// Consent flag.
    pub consent: bool,
    /// Whether enabling the microphone can still show a permission prompt.
    pub can_prompt: bool,
    /// Microphone path: `off`, `awaiting_permission`, `open`, `lost` or `suspended`.
    pub microphone: &'static str,
    /// The open device's name.
    pub device: Option<String>,
    /// The codec new packets are encoded with (`pcm` or `opus`).
    pub codec: &'static str,
    /// Current target bit rate in bits per second.
    pub bitrate_bps: u32,
    /// Mode name.
    pub mode: &'static str,
    /// Quality name.
    pub quality: &'static str,
    /// Codec sample rate.
    pub rate: u32,
    /// `(id, name, kind, joined, active)` per defined channel.
    pub channels: Vec<(VoiceChannelId, String, &'static str, bool, bool)>,
    /// Remote speakers.
    pub speakers: Vec<SpeakerStatus>,
    /// Loopback is on.
    pub loopback: bool,
    /// A synthetic source replaces the microphone.
    pub test_mode: bool,
    /// Processing stages actually in effect.
    pub effective_processing: Vec<&'static str>,
    /// Error codes the running session met, one record per code, sorted by code.
    pub errors: Vec<StreamErrorRecord>,
}

struct FakeSpeaker {
    source: TestSource,
    codec: Box<dyn VoiceCodec + Send>,
    id: VoiceCodecId,
    seq: u16,
}

/// The local participant (spec resource `VoiceChatSession`). Codecs are `Send` but not `Sync`,
/// so worlds hold it in a [`VoiceChatResource`].
pub struct VoiceChatSession {
    settings: VoiceChatSettings,
    user: VoiceChatUserSettings,
    local_sender: u16,
    joined: BTreeSet<VoiceChannelId>,
    active: Option<VoiceChannelId>,
    tx: TransmitMachine,
    gate: TalkGate,
    processing: ProcessingChain,
    reference_frame: Vec<f32>,
    converter: Option<CaptureConverter>,
    device: Option<InputDevice>,
    provider: SharedCodecProvider,
    encoder: Box<dyn VoiceCodec + Send>,
    tx_codec: VoiceCodecId,
    payload: Vec<u8>,
    mic: Option<Box<dyn InputDeviceOpener + Send>>,
    mic_phase: MicPhase,
    mic_samples: Vec<f32>,
    can_prompt: bool,
    reopen_left: u32,
    pending_reset: bool,
    pending_quality: Option<VoiceQuality>,
    bitrate_bps: u32,
    link: Option<VoiceLinkQuality>,
    /// The first frame of a two-frame packet: its sequence number, timestamp, payload and
    /// whether the encoder restarted for it (the pair must carry RESET then).
    held: Option<(u16, u32, Vec<u8>, bool)>,
    /// The last frame sent, for `redundancy`.
    last_frame: Option<(u16, u32, Vec<u8>)>,
    multi: Vec<u8>,
    silent_frames: u32,
    control: VecDeque<VoiceControl>,
    output_tap: bool,
    output: Vec<(u16, u32, Vec<f32>)>,
    seq: u16,
    frame: u64,
    test_source: Option<TestSource>,
    fakes: BTreeMap<u16, FakeSpeaker>,
    loopback_since: Option<u64>,
    network: VoiceChatSimulator,
    speakers: BTreeMap<u16, VoiceSpeaker>,
    last_channel: BTreeMap<u16, u16>,
    audible: BTreeSet<u16>,
    muted_senders: BTreeSet<u16>,
    /// Sender slots the server was told to stop forwarding because their account is muted
    /// (spec 59 § 2.13: an account mute also sends a `mute_hint`).
    account_hinted: BTreeSet<u16>,
    sender_accounts: BTreeMap<u16, String>,
    outbox: Vec<Vec<u8>>,
    events: Vec<VoiceChatEvent>,
    playout: Vec<(u16, u32, Vec<f32>)>,
    /// How many leading [`Self::playout`] slots the last tick filled.
    playout_len: usize,
    /// How many leading [`Self::output`] slots the last tick filled.
    output_len: usize,
    /// PCM scratch for scripted speakers.
    fake_pcm: Vec<f32>,
    /// Wire bytes of the packet currently being written.
    wire: Vec<u8>,
    /// The packet just polled from the simulator.
    recv_buf: Vec<u8>,
    /// Ranking scratch (candidates, the current audible set, the sort, the keep list).
    rank_cands: Vec<Candidate>,
    rank_current: Vec<u16>,
    rank_sorted: Vec<Candidate>,
    rank_kept: Vec<u16>,
    report_ring: ReportRing,
    stats: SessionStats,
    last_state: &'static str,
    test_mode_allowed: bool,
    /// Per-sender debounce of `SpeakerActivity` (spec § 2.14).
    activity: BTreeMap<u16, ActivityDebounce>,
    /// `DecodedFrame` events are sent (spec § 2.14 `emit_decoded_frames`).
    emit_decoded: bool,
    /// `DecodedFrame` events queued since the last [`Self::take_events`].
    decoded_pending: usize,
    /// `code -> (count, last frame)` of the errors the session met (spec § 5).
    errors: BTreeMap<&'static str, (u64, u64)>,
}

impl VoiceChatSession {
    /// A session in `Off` with no channel joined.
    ///
    /// # Errors
    /// Whatever [`VoiceChatSettings::validate`] reports.
    pub fn new(settings: VoiceChatSettings, user: VoiceChatUserSettings) -> Result<Self, VoiceChatError> {
        settings.validate()?;
        let network = VoiceChatSimulator::new(SimulatorSettings::default())?;
        let gate = TalkGate::new(user.mode, settings.vad);
        let provider: SharedCodecProvider = Arc::new(BuiltinCodecs);
        let encoder = provider.build(VoiceCodecId::Pcm, settings.quality, settings.complexity)?;
        Ok(Self {
            provider,
            encoder,
            tx_codec: VoiceCodecId::Pcm,
            payload: vec![0u8; MAX_PCM_PAYLOAD],
            mic: None,
            mic_phase: MicPhase::Off,
            mic_samples: Vec::new(),
            can_prompt: true,
            reopen_left: 0,
            pending_reset: false,
            pending_quality: None,
            bitrate_bps: settings.quality.target_bps(),
            link: None,
            held: None,
            last_frame: None,
            multi: Vec::new(),
            silent_frames: 0,
            control: VecDeque::new(),
            output_tap: false,
            output: Vec::new(),
            report_ring: ReportRing::new(settings.report_buffer_seconds),
            processing: ProcessingChain::new(settings.processing, settings.quality.sample_rate()),
            reference_frame: vec![0.0; settings.quality.frame_samples()],
            settings,
            user,
            local_sender: 0,
            joined: BTreeSet::new(),
            active: None,
            tx: TransmitMachine::default(),
            gate,
            converter: None,
            device: None,
            seq: 0,
            frame: 0,
            test_source: None,
            fakes: BTreeMap::new(),
            loopback_since: None,
            network,
            speakers: BTreeMap::new(),
            last_channel: BTreeMap::new(),
            audible: BTreeSet::new(),
            muted_senders: BTreeSet::new(),
            account_hinted: BTreeSet::new(),
            sender_accounts: BTreeMap::new(),
            outbox: Vec::new(),
            events: Vec::new(),
            playout: Vec::new(),
            playout_len: 0,
            output_len: 0,
            fake_pcm: Vec::new(),
            wire: Vec::new(),
            recv_buf: Vec::new(),
            rank_cands: Vec::new(),
            rank_current: Vec::new(),
            rank_sorted: Vec::new(),
            rank_kept: Vec::new(),
            stats: SessionStats::default(),
            last_state: "off",
            test_mode_allowed: true,
            activity: BTreeMap::new(),
            emit_decoded: false,
            decoded_pending: 0,
            errors: BTreeMap::new(),
        })
    }

    /// The project settings.
    #[must_use]
    pub fn settings(&self) -> &VoiceChatSettings {
        &self.settings
    }

    /// The persisted user settings.
    #[must_use]
    pub fn user(&self) -> &VoiceChatUserSettings {
        &self.user
    }

    /// Mutable user settings (the game's settings screen). Consent is only ever set here, by a
    /// person; the agent protocol has no path to it.
    pub fn user_mut(&mut self) -> &mut VoiceChatUserSettings {
        &mut self.user
    }

    /// Installs the codec provider (`regia_media::voice_opus` for Opus). New talk spurts use its
    /// preferred codec; when that cannot be built the session stays on its current codec.
    ///
    /// # Errors
    /// The provider's error; the current codec stays.
    pub fn set_codec_provider(&mut self, provider: SharedCodecProvider) -> Result<(), VoiceChatError> {
        let id = provider.preferred();
        let encoder = provider.build(id, self.settings.quality, self.settings.complexity)?;
        self.provider = provider;
        self.encoder = encoder;
        self.tx_codec = id;
        self.pending_reset = true;
        self.bitrate_bps = self.settings.quality.target_bps();
        self.encoder.set_target_bitrate(self.bitrate_bps);
        for (sender, fake) in &mut self.fakes {
            match self
                .provider
                .build(id, self.settings.quality, self.settings.complexity)
            {
                Ok(codec) => {
                    fake.codec = codec;
                    fake.id = id;
                }
                Err(e) => tracing::warn!(target: "regia_audio", sender, "fake speaker keeps its codec: {e}"),
            }
        }
        self.speakers.clear();
        self.audible.clear();
        Ok(())
    }

    /// The codec new packets use.
    #[must_use]
    pub fn codec(&self) -> VoiceCodecId {
        self.tx_codec
    }

    /// Sets the transmit mode (the settings screen), clearing the talk latches.
    pub fn set_mode(&mut self, mode: VoiceMode) {
        self.user.mode = mode;
        self.gate.set_mode(mode);
    }

    /// Switches the bandwidth profile at the next point the local player is not talking (no
    /// change happens mid-talk, spec § 2.8), so the server's `quality_cap` can move it.
    pub fn set_quality(&mut self, quality: VoiceQuality) {
        self.pending_quality = (quality != self.settings.quality).then_some(quality);
    }

    fn apply_pending_quality(&mut self) {
        if matches!(*self.tx.state(), TxState::Transmitting | TxState::Hangover) {
            return;
        }
        let Some(quality) = self.pending_quality.take() else {
            return;
        };
        match self
            .provider
            .build(self.tx_codec, quality, self.settings.complexity)
        {
            Ok(encoder) => {
                self.encoder = encoder;
                self.settings.quality = quality;
                self.rebuild_processing(quality);
                self.bitrate_bps = quality.target_bps();
                self.pending_reset = true;
                if let Some(dev) = &self.device {
                    match CaptureConverter::new(dev.rate, dev.channels, quality) {
                        Ok(c) => self.converter = Some(c),
                        Err(e) => {
                            tracing::warn!(target: "regia_audio", "voice chat capture rebuild failed: {e}");
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(target: "regia_audio", "voice chat quality change failed: {e}"),
        }
    }

    /// Whether the developer test mode (synthetic sources, the network simulator) is allowed.
    /// On by default; a shipping game turns it off and the agent methods answer `test_mode_only`.
    #[must_use]
    pub fn test_mode_allowed(&self) -> bool {
        self.test_mode_allowed
    }

    /// Allows or forbids the developer test mode.
    pub fn set_test_mode_allowed(&mut self, allowed: bool) {
        self.test_mode_allowed = allowed;
    }

    /// Sets the local session slot (the server overwrites the sender field anyway).
    pub fn set_local_sender(&mut self, sender: u16) {
        self.local_sender = sender;
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> SessionStats {
        self.stats
    }

    /// The transmit state.
    #[must_use]
    pub fn tx_state(&self) -> &TxState {
        self.tx.state()
    }

    /// Resets the counters and the error record.
    pub fn reset_stats(&mut self) {
        self.stats = SessionStats::default();
        self.errors.clear();
    }

    /// Takes the queued events.
    pub fn take_events(&mut self) -> Vec<VoiceChatEvent> {
        self.decoded_pending = 0;
        std::mem::take(&mut self.events)
    }

    /// Turns `DecodedFrame` events on or off (spec § 2.14 `emit_decoded_frames`, off by default).
    /// Each heard speaker then sends its decoded mono frame every 20 ms; at most
    /// [`MAX_PENDING_DECODED_FRAMES`] wait for [`Self::take_events`].
    pub fn set_emit_decoded_frames(&mut self, on: bool) {
        self.emit_decoded = on;
    }

    /// Whether `DecodedFrame` events are sent.
    #[must_use]
    pub fn emit_decoded_frames(&self) -> bool {
        self.emit_decoded
    }

    /// The server forced a mute on the local sender until `until_tick` (the net layer's
    /// `ServerMuted` notice, spec § 2.13): an `Error(ServerMuted)` event and a `server_muted`
    /// record. `0` lifts a mute and reports nothing.
    pub fn note_server_muted(&mut self, until_tick: u32) {
        if until_tick != 0 {
            self.push_error(VoiceChatError::ServerMuted);
        }
    }

    /// An error event for the game, also kept in the error record.
    fn push_error(&mut self, error: VoiceChatError) {
        record_error(&mut self.errors, error.code(), self.frame);
        self.events.push(VoiceChatEvent::Error(error));
    }

    /// Takes the packets produced since the last call, to hand to a transport.
    pub fn take_outgoing(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outbox)
    }

    /// The frames played out by the last tick: `(sender, sample_rate, mono samples)`, ascending
    /// by sender so the mix order is reproducible (spec § 7). Slots kept for the next tick are
    /// not included.
    #[must_use]
    pub fn playout(&self) -> &[(u16, u32, Vec<f32>)] {
        let n = self.playout_len.min(self.playout.len());
        &self.playout[..n]
    }

    // ----- consent and device -------------------------------------------------------------

    /// Opens the capture device through `opener`. Without consent nothing is opened.
    ///
    /// # Errors
    /// [`VoiceChatError::ConsentRequired`] without consent; the opener's error otherwise (the
    /// state then becomes `Error`).
    pub fn start_capture(&mut self, opener: &mut dyn InputDeviceOpener) -> Result<(), VoiceChatError> {
        if !self.user.consent {
            return Err(VoiceChatError::ConsentRequired);
        }
        let result = opener.open(self.user.device.as_deref());
        match result {
            Ok(dev) => {
                let converter = CaptureConverter::new(dev.rate, dev.channels, self.settings.quality)?;
                self.converter = Some(converter);
                self.device = Some(dev);
                self.tx.device_opened(Ok(()));
                self.events
                    .push(VoiceChatEvent::PermissionResult { granted: true });
                Ok(())
            }
            Err(e) => {
                self.tx.device_opened(Err(e.clone()));
                if e == VoiceChatError::PermissionDenied {
                    self.events
                        .push(VoiceChatEvent::PermissionResult { granted: false });
                }
                Err(e)
            }
        }
    }

    /// Hands the session its microphone. Nothing is opened: that takes consent and
    /// [`enable_microphone`](Self::enable_microphone).
    pub fn set_microphone(&mut self, mic: Box<dyn InputDeviceOpener + Send>) {
        self.close_microphone();
        self.mic = Some(mic);
    }

    /// Whether a microphone is installed.
    #[must_use]
    pub fn has_microphone(&self) -> bool {
        self.mic.is_some()
    }

    /// The explicit enable action of the settings screen: checks consent, runs the permission flow
    /// (the prompt appears here and nowhere else) and opens the device once it is allowed. With a
    /// prompt on screen this returns `Ok` and the session finishes the open from [`tick`](Self::tick).
    ///
    /// # Errors
    /// [`VoiceChatError::ConsentRequired`] without consent, [`VoiceChatError::NoInputDevice`] with
    /// no microphone installed, [`VoiceChatError::PermissionDenied`] when the permission is refused
    /// for good, or the device's open error. The state becomes `Error` for the last two.
    pub fn enable_microphone(&mut self) -> Result<(), VoiceChatError> {
        if !self.user.consent {
            return Err(VoiceChatError::ConsentRequired);
        }
        let Some(mic) = self.mic.as_mut() else {
            return Err(VoiceChatError::NoInputDevice);
        };
        match mic.permission() {
            MicPermission::Granted => self.open_microphone(),
            MicPermission::Pending => {
                self.mic_phase = MicPhase::AwaitingPermission;
                Ok(())
            }
            MicPermission::NotAsked | MicPermission::Denied { can_prompt: true } => {
                mic.request_permission();
                self.mic_phase = MicPhase::AwaitingPermission;
                Ok(())
            }
            MicPermission::Denied { can_prompt: false } => {
                self.can_prompt = false;
                self.permission_refused();
                Err(VoiceChatError::PermissionDenied)
            }
        }
    }

    /// Closes the microphone and returns to `Off` (the player turned voice chat off).
    pub fn disable_microphone(&mut self) {
        self.close_microphone();
    }

    /// The app is pausing: the stream closes (Android drops it, spec § 9) and comes back on
    /// [`resume_microphone`](Self::resume_microphone).
    pub fn suspend_microphone(&mut self) {
        if matches!(self.mic_phase, MicPhase::Open | MicPhase::Lost) {
            self.close_microphone();
            self.mic_phase = MicPhase::Suspended;
        }
    }

    /// The app resumed: reopens a suspended stream.
    pub fn resume_microphone(&mut self) {
        if self.mic_phase != MicPhase::Suspended {
            return;
        }
        self.mic_phase = MicPhase::Off;
        if self.user.consent
            && let Err(e) = self.open_microphone()
        {
            self.push_error(e);
        }
    }

    fn permission_refused(&mut self) {
        self.mic_phase = MicPhase::Off;
        self.tx.device_opened(Err(VoiceChatError::PermissionDenied));
        self.events
            .push(VoiceChatEvent::PermissionResult { granted: false });
    }

    fn open_microphone(&mut self) -> Result<(), VoiceChatError> {
        let Some(mic) = self.mic.as_mut() else {
            return Err(VoiceChatError::NoInputDevice);
        };
        match mic.open(self.user.device.as_deref()) {
            Ok(dev) => {
                let converter = match CaptureConverter::new(dev.rate, dev.channels, self.settings.quality) {
                    Ok(c) => c,
                    Err(e) => {
                        mic.close();
                        self.tx.device_opened(Err(e.clone()));
                        return Err(e);
                    }
                };
                self.converter = Some(converter);
                self.device = Some(dev);
                self.mic_phase = MicPhase::Open;
                self.pending_reset = true;
                self.tx.device_opened(Ok(()));
                self.events
                    .push(VoiceChatEvent::PermissionResult { granted: true });
                Ok(())
            }
            Err(e) => {
                self.mic_phase = MicPhase::Off;
                self.tx.device_opened(Err(e.clone()));
                if e == VoiceChatError::PermissionDenied {
                    self.can_prompt = false;
                    self.events
                        .push(VoiceChatEvent::PermissionResult { granted: false });
                }
                Err(e)
            }
        }
    }

    fn close_microphone(&mut self) {
        if let Some(mic) = self.mic.as_mut() {
            mic.close();
        }
        let was_open = self.mic_phase != MicPhase::Off;
        self.mic_phase = MicPhase::Off;
        self.converter = None;
        self.device = None;
        if was_open && self.test_source.is_none() && self.tx.device_closed() {
            self.send_end_of_talk();
        }
    }

    fn lose_microphone(&mut self) {
        if let Some(mic) = self.mic.as_mut() {
            mic.close();
        }
        self.mic_phase = MicPhase::Lost;
        self.reopen_left = RETRY_FRAMES;
        self.device_removed();
    }

    /// One frame of the microphone path: the permission poll, the capture read, the retry.
    fn tick_microphone(&mut self) {
        match self.mic_phase {
            MicPhase::Off | MicPhase::Suspended => {}
            MicPhase::AwaitingPermission => {
                let Some(mic) = self.mic.as_mut() else {
                    self.mic_phase = MicPhase::Off;
                    return;
                };
                match mic.permission() {
                    MicPermission::Granted => {
                        if let Err(e) = self.open_microphone() {
                            self.push_error(e);
                        }
                    }
                    MicPermission::Denied { can_prompt } => {
                        self.can_prompt = can_prompt;
                        self.permission_refused();
                    }
                    MicPermission::Pending | MicPermission::NotAsked => {}
                }
            }
            MicPhase::Open => {
                if !self.user.consent {
                    // Consent withdrawn: the stream closes in the same frame (spec § 2.15).
                    self.close_microphone();
                    return;
                }
                let Some(mic) = self.mic.as_mut() else {
                    return;
                };
                self.mic_samples.clear();
                let read = mic.read(&mut self.mic_samples);
                if read.is_err() {
                    self.lose_microphone();
                    return;
                }
                if let Some(c) = &mut self.converter {
                    c.push(&self.mic_samples);
                    while c.frames_waiting() > MAX_CAPTURE_BACKLOG_FRAMES {
                        c.pop_frame();
                        self.stats.capture_overruns += 1;
                    }
                }
            }
            MicPhase::Lost => {
                self.reopen_left = self.reopen_left.saturating_sub(1);
                if self.reopen_left > 0 {
                    return;
                }
                self.reopen_left = RETRY_FRAMES;
                let Some(mic) = self.mic.as_mut() else {
                    return;
                };
                if !self.user.consent {
                    return;
                }
                if let Ok(dev) = mic.open(self.user.device.as_deref()) {
                    match CaptureConverter::new(dev.rate, dev.channels, self.settings.quality) {
                        Ok(c) => {
                            self.converter = Some(c);
                            self.device = Some(dev);
                            self.mic_phase = MicPhase::Open;
                            self.pending_reset = true;
                        }
                        Err(_) => mic.close(),
                    }
                }
            }
        }
    }

    /// Feeds interleaved device samples (any chunk size) into the capture converter.
    pub fn push_capture(&mut self, interleaved: &[f32]) {
        if let Some(c) = &mut self.converter {
            c.push(interleaved);
        }
    }

    /// The device vanished (hot-unplug).
    pub fn device_removed(&mut self) {
        self.converter = None;
        self.device = None;
        if self.tx.device_removed() {
            self.send_end_of_talk();
        }
    }

    /// Developer test mode: a synthetic source replaces the microphone and moves a `Off` machine
    /// to `Idle` without consent, because the device is never opened.
    pub fn set_test_source(&mut self, source: Option<TestSource>) {
        if source.is_some() && *self.tx.state() == TxState::Off {
            self.tx.device_opened(Ok(()));
        }
        self.test_source = source;
    }

    /// Adds a fake remote speaker fed by a synthetic source, or removes it with `None`.
    pub fn set_fake_speaker(&mut self, sender: u16, source: Option<TestSource>) {
        match source {
            Some(source) => {
                let id = self.tx_codec;
                match self
                    .provider
                    .build(id, self.settings.quality, self.settings.complexity)
                {
                    Ok(codec) => {
                        self.fakes.insert(
                            sender,
                            FakeSpeaker {
                                source,
                                codec,
                                id,
                                seq: 0,
                            },
                        );
                    }
                    Err(e) => self.push_error(e),
                }
            }
            None => {
                self.fakes.remove(&sender);
            }
        }
    }

    // ----- channels ------------------------------------------------------------------------

    fn channel(&self, id: VoiceChannelId) -> Result<&VoiceChannel, VoiceChatError> {
        self.settings
            .channels
            .iter()
            .find(|c| c.id == id)
            .ok_or(VoiceChatError::UnknownChannel(id))
    }

    /// Joins or leaves a channel and/or makes it the transmit channel. Returns whether it is
    /// joined and active afterwards. A radius overrides the channel's radius.
    ///
    /// # Errors
    /// [`VoiceChatError::UnknownChannel`], [`VoiceChatError::NotInChannel`] when activating a
    /// channel that is not joined, [`VoiceChatError::InvalidParams`] for a bad radius.
    pub fn set_channel(
        &mut self,
        id: VoiceChannelId,
        join: Option<bool>,
        active: Option<bool>,
        radius: Option<f32>,
    ) -> Result<(bool, bool), VoiceChatError> {
        self.channel(id)?;
        if let Some(r) = radius
            && (!r.is_finite() || r < 0.0)
        {
            return Err(VoiceChatError::InvalidParams(
                "radius must be finite and not negative".into(),
            ));
        }
        if let Some(join) = join {
            if join {
                if self.joined.insert(id) {
                    self.queue_control(VoiceControl::Join(id));
                }
            } else {
                if self.joined.remove(&id) {
                    self.queue_control(VoiceControl::Leave(id));
                }
                if self.active == Some(id) {
                    self.active = None;
                }
            }
        }
        if let Some(active) = active {
            if active {
                if !self.joined.contains(&id) {
                    return Err(VoiceChatError::NotInChannel(id));
                }
                self.active = Some(id);
            } else if self.active == Some(id) {
                self.active = None;
            }
        }
        if let (Some(r), Some(c)) = (radius, self.settings.channels.iter_mut().find(|c| c.id == id)) {
            c.radius = r;
        }
        Ok((self.joined.contains(&id), self.active == Some(id)))
    }

    fn queue_control(&mut self, message: VoiceControl) {
        if self.control.len() >= MAX_PENDING_CONTROL {
            self.control.pop_front();
        }
        self.control.push_back(message);
    }

    /// Takes the membership and moderation changes for the server.
    pub fn take_control(&mut self) -> Vec<VoiceControl> {
        self.control.drain(..).collect()
    }

    /// The channels this player is in, ascending (a transport re-sends them after a reconnect).
    #[must_use]
    pub fn joined_channels(&self) -> Vec<VoiceChannelId> {
        self.joined.iter().copied().collect()
    }

    /// The channel a speaker last sent on.
    #[must_use]
    pub fn speaker_channel(&self, sender: u16) -> Option<VoiceChannelId> {
        self.last_channel.get(&sender).copied()
    }

    /// The transmit channel.
    #[must_use]
    pub fn active_channel(&self) -> Option<VoiceChannelId> {
        self.active
    }

    // ----- moderation ----------------------------------------------------------------------

    /// Self mute. Sends `end_of_talk` when mid-speech.
    pub fn set_self_muted(&mut self, muted: bool) {
        if self.tx.set_muted(muted) {
            self.send_end_of_talk();
        }
    }

    /// Deafen: silences all playback and implies self mute.
    pub fn set_deafened(&mut self, deafen: bool) {
        self.user.deafen = deafen;
        if deafen {
            self.set_self_muted(true);
        }
    }

    /// Locally mutes or unmutes a sender slot; returns the `mute_hint` the transport should send
    /// to the server.
    pub fn mute_sender(&mut self, sender: u16, muted: bool) -> (u16, bool) {
        if let Some(s) = self.speakers.get_mut(&sender) {
            s.muted = muted;
            if muted {
                s.reset();
            }
        }
        if muted {
            self.muted_senders.insert(sender);
        } else {
            self.muted_senders.remove(&sender);
        }
        // A muted account keeps its slot hinted: unmuting the slot alone must not ask the server
        // to forward a speaker this listener still drops.
        let hinted = muted || self.account_hinted.contains(&sender);
        self.queue_control(VoiceControl::MuteHint {
            sender,
            muted: hinted,
        });
        (sender, hinted)
    }

    /// Mutes an account (persisted in the user settings, evicting the oldest above the cap).
    pub fn mute_account(&mut self, account: &str, note: &str, muted: bool) {
        if muted {
            self.user.muted_accounts.add(account, note);
        } else {
            self.user.muted_accounts.remove(account);
        }
        self.apply_account_lists();
    }

    /// Blocks an account (persisted).
    pub fn block_account(&mut self, account: &str, note: &str, blocked: bool) {
        if blocked {
            self.user.blocked_accounts.add(account, note);
        } else {
            self.user.blocked_accounts.remove(account);
        }
        self.queue_control(VoiceControl::Block {
            account: account.to_string(),
            blocked,
        });
        self.apply_account_lists();
    }

    /// Tells the session which account a sender slot belongs to (from the session rebinding
    /// announcement). A reused slot with a different account is reset.
    /// At most [`MAX_BOUND_SENDERS`] slots are bound; a new one past it is ignored. Only this
    /// slot's speaker is re-checked against the mute and block lists: re-checking every bound
    /// slot made each notice cost the whole roster, so a server sending one notice per slot
    /// cost the square of its slot count (ADR-5870).
    pub fn bind_sender(&mut self, sender: u16, account: &str) {
        let known = self.sender_accounts.get(&sender);
        if known.is_none() && self.sender_accounts.len() >= MAX_BOUND_SENDERS {
            return;
        }
        let changed = known.is_some_and(|a| a != account);
        if changed && let Some(s) = self.speakers.get_mut(&sender) {
            s.reset();
            s.muted = false;
        }
        self.sender_accounts.insert(sender, account.to_string());
        // The speaker's mute and, for a muted account, its `mute_hint` (spec 59 § 2.13, ADR-5642):
        // the integ-v049 merge of sec-review-net-mods' cap and fixrev5-voice-openxr kept the cap
        // and lost this call (ADR-5993).
        self.apply_account_lists();
    }

    /// A slot was freed: its speaker and account binding go.
    pub fn unbind_sender(&mut self, sender: u16) {
        self.sender_accounts.remove(&sender);
        self.speakers.remove(&sender);
        self.audible.remove(&sender);
        self.last_channel.remove(&sender);
        self.muted_senders.remove(&sender);
        self.account_hinted.remove(&sender);
    }

    fn apply_account_lists(&mut self) {
        let user = &self.user;
        let mut hints = Vec::new();
        for (sender, account) in &self.sender_accounts {
            let account_muted = user.muted_accounts.contains(account);
            let bad = account_muted || user.blocked_accounts.contains(account);
            if let Some(s) = self.speakers.get_mut(sender) {
                s.muted = bad || self.muted_senders.contains(sender);
            }
            // Spec 59 § 2.13: an account mute is also a `mute_hint` for every slot of that
            // account, sent once per change (a block is enforced by the server already).
            if account_muted != self.account_hinted.contains(sender) {
                hints.push((*sender, account_muted));
            }
        }
        for (sender, account_muted) in hints {
            if account_muted {
                self.account_hinted.insert(sender);
            } else {
                self.account_hinted.remove(&sender);
            }
            // Unmuting the account of a slot muted on its own changes nothing on the server.
            if account_muted || !self.muted_senders.contains(&sender) {
                self.queue_control(VoiceControl::MuteHint {
                    sender,
                    muted: account_muted,
                });
            }
        }
    }

    /// Whether a sender slot is muted or blocked here.
    #[must_use]
    pub fn is_sender_silenced(&self, sender: u16) -> bool {
        self.muted_senders.contains(&sender)
            || self.sender_accounts.get(&sender).is_some_and(|a| {
                self.user.muted_accounts.contains(a) || self.user.blocked_accounts.contains(a)
            })
    }

    /// Per-speaker volume in percent, 0..=200.
    ///
    /// # Errors
    /// [`VoiceChatError::InvalidParams`] outside the range, [`VoiceChatError::UnknownSpeaker`].
    pub fn set_speaker_volume(&mut self, sender: u16, percent: f32) -> Result<(), VoiceChatError> {
        if !percent.is_finite() || !(0.0..=200.0).contains(&percent) {
            return Err(VoiceChatError::InvalidParams("volume must be in 0..=200".into()));
        }
        let s = self
            .speakers
            .get_mut(&sender)
            .ok_or(VoiceChatError::UnknownSpeaker(sender))?;
        s.gain = percent / 100.0;
        Ok(())
    }

    /// The game's UI asked to report `sender`: hands the buffered encoded packets to `sink`.
    /// Returns whether anything was buffered (never when `report_buffer_seconds` is 0).
    pub fn report(&mut self, sender: u16, sink: &mut dyn VoiceReportSink) -> bool {
        let Some(report): Option<VoiceReport> = self.report_ring.report(sender) else {
            return false;
        };
        self.events.push(VoiceChatEvent::ReportRequested {
            sender,
            channel: report.channel,
            seconds: self.settings.report_buffer_seconds,
        });
        sink.submit(report);
        true
    }

    /// Ends the session: clears the report ring and every speaker. A speaker whose last
    /// `SpeakerActivity` said "started" gets its "stopped" event now.
    pub fn end_session(&mut self) {
        self.report_ring.clear();
        for (sender, state) in std::mem::take(&mut self.activity) {
            if state.reported {
                self.events.push(VoiceChatEvent::SpeakerActivity {
                    sender,
                    channel: self.last_channel.get(&sender).copied().unwrap_or(0),
                    active: false,
                    level: 0.0,
                });
            }
        }
        self.speakers.clear();
        self.audible.clear();
        self.loopback_since = None;
    }

    // ----- network simulation and loopback -------------------------------------------------

    /// Applies the packet-loss simulator settings used by loopback and fake speakers.
    ///
    /// # Errors
    /// [`VoiceChatError::InvalidParams`].
    pub fn simulate_network(&mut self, settings: SimulatorSettings) -> Result<(), VoiceChatError> {
        self.network.apply(settings)
    }

    /// Turns the full-chain loopback on or off. With `mic` the caller must have consent.
    ///
    /// # Errors
    /// [`VoiceChatError::ConsentRequired`] for a microphone source without consent.
    pub fn set_loopback(&mut self, enabled: bool, mic: bool) -> Result<(), VoiceChatError> {
        if enabled {
            if mic && !self.user.consent {
                return Err(VoiceChatError::ConsentRequired);
            }
            self.loopback_since = Some(self.frame);
        } else {
            self.loopback_since = None;
            self.speakers.remove(&LOOPBACK_SENDER);
        }
        Ok(())
    }

    /// Whether loopback is on.
    #[must_use]
    pub fn loopback(&self) -> bool {
        self.loopback_since.is_some()
    }

    // ----- the frame -----------------------------------------------------------------------

    /// Advances one 20 ms voice frame. `talk_pressed` is the `voice_chat_talk` action. Packets
    /// produced are queued for [`take_outgoing`](Self::take_outgoing) (and, with loopback on, also
    /// sent through the network simulator).
    pub fn tick(&mut self, talk_pressed: bool) {
        let _span = tracing::trace_span!("voice_chat_tick").entered();
        self.frame += 1;
        self.network.advance();
        self.apply_pending_quality();
        self.tick_microphone();
        self.tick_bitrate();
        self.tick_transmit(talk_pressed);
        self.tick_fakes();
        self.tick_loopback_limit();
        loop {
            let mut buf = std::mem::take(&mut self.recv_buf);
            let got = self.network.poll_packet_into(&mut buf);
            if !got {
                self.recv_buf = buf;
                break;
            }
            self.receive_packet(&buf);
            self.recv_buf = buf;
        }
        self.tick_playout();
        let state = self.tx.state().name();
        if state != self.last_state {
            self.last_state = state;
            self.events.push(VoiceChatEvent::StateChanged { state });
        }
    }

    /// Reports the transport's link measurements (the adaptive bit rate's input).
    pub fn set_link_quality(&mut self, quality: VoiceLinkQuality) {
        self.link = Some(quality);
    }

    /// Every 2 s, one step between the profile's floor (60 % of target) and ceiling (the target).
    fn tick_bitrate(&mut self) {
        if !self.frame.is_multiple_of(BITRATE_STEP_FRAMES) {
            return;
        }
        let Some(link) = self.link else {
            return;
        };
        let ceiling = self.settings.quality.target_bps();
        let floor = ceiling / 5 * 3;
        let next = if link.loss_pct > BITRATE_DOWN_LOSS {
            (self.bitrate_bps / 10 * 9).max(floor)
        } else if link.loss_pct < BITRATE_UP_LOSS {
            (self.bitrate_bps / 10 * 11).min(ceiling)
        } else {
            self.bitrate_bps
        };
        self.encoder
            .set_expected_loss(link.loss_pct.clamp(0.0, 100.0) as u8);
        if next != self.bitrate_bps {
            self.bitrate_bps = next;
            self.encoder.set_target_bitrate(next);
        }
    }

    fn next_local_frame(&mut self) -> Option<Vec<f32>> {
        let n = self.settings.quality.frame_samples();
        if let Some(c) = &mut self.converter
            && let Some(f) = c.pop_frame()
        {
            return Some(f);
        }
        let rate = self.settings.quality.sample_rate();
        let src = self.test_source.as_mut()?;
        let mut f = vec![0.0f32; n];
        src.fill(&mut f, rate);
        Some(f)
    }

    fn tick_transmit(&mut self, talk_pressed: bool) {
        self.tx.retry(self.device.is_some() || self.test_source.is_some());
        let Some(mut frame) = self.next_local_frame() else {
            return;
        };
        self.processing.high_pass(&mut frame, self.settings.deterministic);
        let gain = user_gain(self.user.input_gain_db);
        if (gain - 1.0).abs() > f32::EPSILON {
            for s in &mut frame {
                *s *= gain;
            }
        }
        // Echo cancellation, noise suppression and gain (each off unless set, ADR-2608).
        self.processing.process(&mut frame);
        let wants = self.gate.wants_talk(&frame, talk_pressed);
        let out = self.tx.step(wants);
        if !out.transmit {
            return;
        }
        let Some(channel) = self.active else {
            return;
        };
        // An open microphone in silence sends nothing but a comfort packet every 400 ms (spec § 2.2).
        if self.user.mode == VoiceMode::Open
            && !out.end_of_talk
            && frame_rms_db(&frame) < self.settings.vad.off_db
        {
            self.silent_frames += 1;
            // A lone frame waiting for a partner must not be paired with a later spurt.
            self.flush_held(channel);
            if self.silent_frames.is_multiple_of(COMFORT_INTERVAL_FRAMES)
                && let Ok(n) = self.encoder.encode_comfort(&mut self.payload)
            {
                self.send_payload(channel, VoiceFlags::default(), n);
            }
            return;
        }
        self.silent_frames = 0;
        let _span = tracing::trace_span!("voice_chat_encode").entered();
        // `pending_reset` stays set until a packet that actually carries RESET goes out. The
        // flag is consumed only once the encode below succeeded: a frame held for its partner,
        // an encode error or an oversize frame sends nothing, and dropping the flag would leave
        // the remote decoder unaware that the local encoder restarted.
        let reset = self.pending_reset;
        if reset {
            self.encoder.reset();
        }
        let n = match self.encoder.encode(&frame, &mut self.payload) {
            Ok(n) => n,
            Err(e) => {
                self.stats.encode_errors += 1;
                self.push_error(e);
                return;
            }
        };
        let cap = if self.tx_codec == VoiceCodecId::Pcm {
            MAX_PCM_PAYLOAD
        } else {
            MAX_PAYLOAD
        };
        if n > cap {
            self.stats.oversize_frames += 1;
            return;
        }
        // These bytes are post-reset bytes, so the flag is committed: it goes out on the packet
        // that carries them, or on the pair that later holds a held first frame.
        self.pending_reset = false;
        self.stats.frames_encoded += 1;
        let timestamp = self.timestamp();
        if self.settings.frames_per_packet == 2 {
            if let Some((seq, first_ts, first, held_reset)) = self.held.take() {
                self.multi.clear();
                write_multi(&first, &self.payload[..n], false, &mut self.multi);
                self.seq = self.seq.wrapping_add(1);
                let flags = VoiceFlags::default()
                    .with(VoiceFlags::END_OF_TALK, out.end_of_talk)
                    .with(VoiceFlags::RESET, reset || held_reset);
                self.send_multi(channel, flags, seq, first_ts);
                return;
            }
            if !out.end_of_talk {
                // Forty milliseconds per packet: this frame waits for the next one, keeping the
                // reset flag with it.
                self.held = Some((self.seq, timestamp, self.payload[..n].to_vec(), reset));
                self.seq = self.seq.wrapping_add(1);
                return;
            }
        }
        let flags = VoiceFlags::default()
            .with(VoiceFlags::END_OF_TALK, out.end_of_talk)
            .with(VoiceFlags::RESET, reset);
        let loss = self.link.map_or(0.0, |l| l.loss_pct);
        let spare = self
            .last_frame
            .take()
            .filter(|(seq, ..)| {
                self.settings.redundancy && loss > REDUNDANCY_LOSS && seq.wrapping_add(1) == self.seq
            })
            .filter(|(_, _, first)| first.len() + n + 3 <= cap);
        if self.settings.redundancy {
            self.last_frame = Some((self.seq, timestamp, self.payload[..n].to_vec()));
        }
        if let Some((seq, first_ts, first)) = spare {
            self.multi.clear();
            write_multi(&first, &self.payload[..n], true, &mut self.multi);
            self.seq = self.seq.wrapping_add(1);
            self.send_multi(channel, flags, seq, first_ts);
            return;
        }
        self.send_payload(channel, flags, n);
    }

    fn timestamp(&self) -> u32 {
        (self.frame as u32).wrapping_mul(FRAME_TIMESTAMP_STEP)
    }

    fn send_multi(&mut self, channel: VoiceChannelId, flags: VoiceFlags, seq: u16, timestamp: u32) {
        let packet = VoicePacket {
            flags: flags.with(VoiceFlags::MULTI, true),
            sender: self.local_sender,
            channel,
            seq,
            timestamp,
            quality: self.settings.quality,
            codec: self.tx_codec,
            payload: &self.multi,
        };
        let mut wire = Vec::new();
        packet.write(&mut wire);
        self.dispatch(wire);
    }

    fn send_payload(&mut self, channel: VoiceChannelId, flags: VoiceFlags, len: usize) {
        // A game codec that claims more bytes than the buffer holds must not slice out of range.
        if len > self.payload.len() {
            self.stats.encode_errors += 1;
            return;
        }
        let packet = VoicePacket {
            flags,
            sender: self.local_sender,
            channel,
            seq: self.seq,
            timestamp: self.timestamp(),
            quality: self.settings.quality,
            codec: self.tx_codec,
            payload: &self.payload[..len],
        };
        self.seq = self.seq.wrapping_add(1);
        let mut wire = Vec::new();
        packet.write(&mut wire);
        self.dispatch(wire);
    }

    fn dispatch(&mut self, wire: Vec<u8>) {
        self.stats.packets_sent += 1;
        self.stats.bytes_sent += wire.len() as u64;
        if self.loopback_since.is_some() {
            let mut echo = wire.clone();
            echo[1..3].copy_from_slice(&LOOPBACK_SENDER.to_le_bytes());
            self.network.send_packet(&echo);
        }
        self.outbox.push(wire);
    }

    /// A frame waiting for its partner goes out alone, on the sequence number it was given,
    /// keeping the reset flag if the encoder restarted for it.
    fn flush_held(&mut self, channel: u16) {
        let Some((seq, timestamp, first, reset)) = self.held.take() else {
            return;
        };
        let packet = VoicePacket {
            flags: VoiceFlags::default().with(VoiceFlags::RESET, reset),
            sender: self.local_sender,
            channel,
            seq,
            timestamp,
            quality: self.settings.quality,
            codec: self.tx_codec,
            payload: &first,
        };
        let mut wire = Vec::new();
        packet.write(&mut wire);
        self.dispatch(wire);
    }

    fn send_end_of_talk(&mut self) {
        let Some(channel) = self.active else {
            return;
        };
        self.flush_held(channel);
        self.last_frame = None;
        let n = match self.encoder.encode_comfort(&mut self.payload) {
            Ok(n) => n,
            Err(e) => {
                self.stats.encode_errors += 1;
                self.push_error(e);
                return;
            }
        };
        self.send_payload(
            channel,
            VoiceFlags::default().with(VoiceFlags::END_OF_TALK, true),
            n,
        );
    }

    fn tick_fakes(&mut self) {
        let quality = self.settings.quality;
        let channel = self.active.unwrap_or(0);
        let frame = self.frame;
        let samples = quality.frame_samples();
        let rate = quality.sample_rate();
        let VoiceChatSession {
            fakes,
            fake_pcm,
            payload,
            wire,
            network,
            ..
        } = self;
        if fake_pcm.len() != samples {
            fake_pcm.resize(samples, 0.0);
        }
        for (sender, fake) in fakes.iter_mut() {
            fake.source.fill(fake_pcm, rate);
            let Ok(n) = fake.codec.encode(fake_pcm, payload) else {
                continue;
            };
            if n > payload.len() {
                continue;
            }
            let packet = VoicePacket {
                flags: VoiceFlags::default(),
                sender: *sender,
                channel,
                seq: fake.seq,
                timestamp: (frame as u32).wrapping_mul(FRAME_TIMESTAMP_STEP),
                quality,
                codec: fake.id,
                payload: &payload[..n],
            };
            fake.seq = fake.seq.wrapping_add(1);
            wire.clear();
            packet.write(wire);
            network.send_packet(wire);
        }
    }

    fn tick_loopback_limit(&mut self) {
        if let Some(since) = self.loopback_since
            && self.frame.saturating_sub(since) >= LOOPBACK_MAX_FRAMES
        {
            self.loopback_since = None;
            self.speakers.remove(&LOOPBACK_SENDER);
        }
    }

    /// Parses a packet from the transport and routes it into the speaker's jitter buffer.
    pub fn receive_packet(&mut self, bytes: &[u8]) {
        let packet = match VoicePacket::parse(bytes) {
            Ok(p) => p,
            Err(_) => {
                self.stats.bad_packets += 1;
                record_error(&mut self.errors, "bad_packet", self.frame);
                return;
            }
        };
        let sender = packet.sender;
        let loopback = sender == LOOPBACK_SENDER;
        if loopback && self.loopback_since.is_none() {
            return;
        }
        if !loopback && !self.joined.contains(&packet.channel) {
            self.stats.wrong_channel += 1;
            return;
        }
        if self.user.deafen {
            self.stats.deafened_drops += 1;
            return;
        }
        if !loopback && self.is_sender_silenced(sender) {
            self.stats.muted_drops += 1;
            return;
        }
        if !self.speakers.contains_key(&sender) && self.speakers.len() >= MAX_REMOTE_SPEAKERS {
            self.stats.overflow_drops += 1;
            return;
        }
        self.stats.packets_received += 1;
        self.stats.bytes_received += bytes.len() as u64;
        self.report_ring.record(sender, packet.channel, bytes);
        self.last_channel.insert(sender, packet.channel);
        let jitter = self.settings.jitter;
        let provider = &self.provider;
        let complexity = self.settings.complexity;
        let scaling = !self.settings.deterministic;
        let speaker = self.speakers.entry(sender).or_insert_with(|| {
            let mut s = VoiceSpeaker::with_provider(sender, jitter, Arc::clone(provider), complexity);
            s.set_time_scaling(scaling);
            s
        });
        speaker.receive(&packet, self.frame);
    }

    fn tick_playout(&mut self) {
        let _span = tracing::trace_span!("voice_chat_playout").entered();
        if self.frame.is_multiple_of(RANK_INTERVAL_FRAMES) || self.audible.is_empty() {
            let VoiceChatSession {
                speakers,
                rank_cands,
                rank_current,
                rank_sorted,
                rank_kept,
                last_channel,
                settings,
                audible,
                stats,
                ..
            } = self;
            rank_cands.clear();
            for (id, speaker) in speakers.iter() {
                if speaker.state() == SpeakerState::Silent {
                    continue;
                }
                let priority = last_channel
                    .get(id)
                    .and_then(|channel| settings.channels.iter().find(|ch| ch.id == *channel))
                    .map_or(0, |channel| channel.priority);
                rank_cands.push(Candidate {
                    sender: *id,
                    priority,
                    energy_db: speaker.level_db(),
                });
            }
            rank_current.clear();
            rank_current.extend(audible.iter().copied());
            rank_speakers_sticky_into(
                rank_cands,
                usize::from(settings.max_mixed_speakers),
                rank_current,
                RANK_STAY_IN_MARGIN_DB,
                rank_sorted,
                rank_kept,
            );
            let same = audible.len() == rank_kept.len() && rank_kept.iter().all(|id| audible.contains(id));
            if !same {
                audible.clear();
                audible.extend(rank_kept.iter().copied());
            }
            for candidate in rank_cands.iter() {
                if !audible.contains(&candidate.sender)
                    && let Some(speaker) = speakers.get_mut(&candidate.sender)
                {
                    speaker.reset();
                    stats.overflow_drops += 1;
                }
            }
        }
        let frame = self.frame;
        let out_gain = user_gain(self.user.output_gain_db);
        let deaf = self.user.deafen;
        let tap = self.output_tap;
        // Spec 59 § 2.4: with the audible set full, a speaker outside it is not mixed until a
        // re-ranking lets it in (it still decodes, so its energy stays current for the ranking).
        let full = self.audible.len() >= usize::from(self.settings.max_mixed_speakers);
        let emit_decoded = self.emit_decoded;
        {
            let VoiceChatSession {
                speakers,
                playout,
                output,
                audible,
                playout_len,
                output_len,
                activity,
                events,
                decoded_pending,
                last_channel,
                errors,
                ..
            } = self;
            let mut slot = 0usize;
            // The output keeps every frame played since the consumer last acknowledged it: the
            // pump may run several voice ticks per game update (below 50 fps, or after a hitch)
            // and the output system reads once per update, so a tick appends after the frames
            // still pending instead of overwriting them.
            let mut pending = if tap { (*output_len).min(output.len()) } else { 0 };
            for (id, speaker) in speakers.iter_mut() {
                // Judged before this tick's frame is pulled: a speaker outside the mix that is
                // silent is not pulled at all (its packets wait for the next ranking), and one
                // that falls silent on this frame still reports its "stopped speaking" edge.
                if !audible.contains(id) && speaker.state() == SpeakerState::Silent {
                    continue;
                }
                speaker.render(frame);
                if let Some(edge) = speaker.activity_edge() {
                    activity.entry(*id).or_default().pending = Some(edge);
                }
                if speaker.take_codec_error() {
                    record_error(errors, "codec_error", frame);
                }
                // Spec 59 § 2.4: while the audible set is full, a speaker outside it is decoded
                // (fresh energy for the ranking, no packet pile-up) and its activity edge is
                // taken, but it is not mixed.
                if full && !audible.contains(id) {
                    continue;
                }
                let samples = speaker.rendered();
                if samples.is_empty() || deaf {
                    continue;
                }
                let rate = speaker.sample_rate();
                if emit_decoded && *decoded_pending < MAX_PENDING_DECODED_FRAMES {
                    *decoded_pending += 1;
                    events.push(VoiceChatEvent::DecodedFrame {
                        sender: *id,
                        channel: last_channel.get(id).copied().unwrap_or(0),
                        rate,
                        samples: Arc::from(samples),
                    });
                }
                if playout.len() == slot {
                    playout.push((*id, rate, Vec::with_capacity(samples.len())));
                }
                playout[slot].0 = *id;
                playout[slot].1 = rate;
                playout[slot].2.clear();
                playout[slot].2.extend_from_slice(samples);
                if (out_gain - 1.0).abs() > f32::EPSILON {
                    for sample in &mut playout[slot].2 {
                        *sample *= out_gain;
                    }
                }
                if tap && pending < MAX_PENDING_OUTPUT {
                    if output.len() == pending {
                        output.push((*id, rate, Vec::with_capacity(playout[slot].2.len())));
                    }
                    output[pending].0 = *id;
                    output[pending].1 = rate;
                    output[pending].2.clear();
                    output[pending].2.extend_from_slice(&playout[slot].2);
                    pending += 1;
                }
                slot += 1;
            }
            *playout_len = slot;
            *output_len = pending;
        }
        self.feed_echo_reference();
        self.flush_activity();
    }

    /// Sends the debounced `SpeakerActivity` events (spec § 2.14, 10 Hz): per sender, an event at
    /// most every [`ACTIVITY_DEBOUNCE_FRAMES`]. An edge inside the window waits for its end and is
    /// sent then only if the state differs from the last event, so a stop and restart inside
    /// 100 ms sends nothing and the events always alternate, starting with "started".
    fn flush_activity(&mut self) {
        let frame = self.frame;
        let VoiceChatSession {
            activity,
            events,
            last_channel,
            ..
        } = self;
        for (sender, state) in activity.iter_mut() {
            let Some((active, level_db)) = state.pending else {
                continue;
            };
            if state
                .reported_at
                .is_some_and(|at| frame < at.saturating_add(ACTIVITY_DEBOUNCE_FRAMES))
            {
                continue;
            }
            state.pending = None;
            if active == state.reported {
                continue;
            }
            state.reported = active;
            state.reported_at = Some(frame);
            events.push(VoiceChatEvent::SpeakerActivity {
                sender: *sender,
                channel: last_channel.get(sender).copied().unwrap_or(0),
                active,
                level: 10f32.powf(level_db / 20.0).clamp(0.0, 1.0),
            });
        }
        // A sender with nothing to report and whose window has passed needs no entry.
        activity.retain(|_, state| {
            state.reported
                || state.pending.is_some()
                || state
                    .reported_at
                    .is_some_and(|at| frame < at.saturating_add(ACTIVITY_DEBOUNCE_FRAMES))
        });
    }

    /// Starts keeping played frames for [`take_output`](Self::take_output): the audio output
    /// turns it on, so a session with no output keeps nothing.
    pub fn set_output_tap(&mut self, on: bool) {
        self.output_tap = on;
        if self.processing.reference_source() == EchoReference::Playout {
            self.processing.set_reference_attached(on);
        }
        if !on {
            self.output.clear();
        }
    }

    /// Every frame played since the last call, in play order: `(sender, sample_rate, mono)`.
    pub fn take_output(&mut self) -> Vec<(u16, u32, Vec<f32>)> {
        let n = self.output_len.min(self.output.len());
        let out = self.output.drain(..n).collect();
        self.output_len = 0;
        out
    }

    /// Frames played since the last [`Self::acknowledge_output`] (or [`Self::take_output`]), in
    /// play order, while the output tap is on: several ticks' frames when the pump ran more than
    /// one tick. The buffers stay with the session, so the ticks after an acknowledge reuse them.
    #[must_use]
    pub fn played(&self) -> &[(u16, u32, Vec<f32>)] {
        let n = self.output_len.min(self.output.len());
        &self.output[..n]
    }

    /// Drops the frames [`Self::played`] returned without freeing the sample buffers.
    pub fn acknowledge_output(&mut self) {
        self.output_len = 0;
    }

    /// Writes the senders that placement walks, or the configured channel ids when the session
    /// has no speakers. Channel names are not cloned. `out` keeps its capacity.
    pub fn place_speakers(&self, out: &mut Vec<u16>) {
        out.clear();
        if self.speakers.is_empty() {
            for channel in &self.settings.channels {
                out.push(channel.id);
            }
            return;
        }
        for id in self.speakers.keys() {
            out.push(*id);
        }
    }

    /// Every remote speaker, ascending by sender.
    pub fn iter_speakers(&self) -> impl Iterator<Item = (u16, &VoiceSpeaker)> + '_ {
        self.speakers.iter().map(|(id, speaker)| (*id, speaker))
    }

    /// One frame of the session's playout, mixed to mono at the capture rate, into the echo
    /// reference (source `Playout`, while an output is attached). Pushed every frame, silence
    /// included, so the reference stays aligned with the capture.
    fn feed_echo_reference(&mut self) {
        if !self.processing.echo_cancel_effective()
            || self.processing.reference_source() != EchoReference::Playout
        {
            return;
        }
        let rate = self.processing.sample_rate();
        self.reference_frame.fill(0.0);
        for (_, src_rate, samples) in self.playout.iter().take(self.playout_len) {
            mix_into(&mut self.reference_frame, samples, *src_rate, rate);
        }
        self.processing.push_reference(&self.reference_frame);
    }

    fn rebuild_processing(&mut self, quality: VoiceQuality) {
        let source = self.processing.reference_source();
        let mut chain = ProcessingChain::new(self.settings.processing, quality.sample_rate());
        chain.set_reference_source(source);
        if source == EchoReference::Playout {
            chain.set_reference_attached(self.output_tap);
        }
        self.processing = chain;
        self.reference_frame = vec![0.0; quality.frame_samples()];
    }

    // ----- processing (spec § 2.7, ADR-2608) ----------------------------------------------

    /// Turns the optional processing stages on or off (`voice_chat.processing`). A stage turned
    /// on starts fresh; one that stays on keeps what it learned. The project settings follow, so
    /// [`settings`](Self::settings) reports the change.
    pub fn set_processing(&mut self, processing: ProcessingSettings) {
        self.settings.processing = processing;
        self.processing.configure(processing);
    }

    /// The processing chain (requested settings, effective stages, reference state).
    #[must_use]
    pub fn processing(&self) -> &ProcessingChain {
        &self.processing
    }

    /// Per-stage counters and measured cost of the processing chain.
    #[must_use]
    pub fn processing_stats(&self) -> ProcessingStats {
        self.processing.stats()
    }

    /// Clears the processing counters.
    pub fn reset_processing_stats(&mut self) {
        self.processing.reset_stats();
    }

    /// Chooses where echo cancellation's far-end reference comes from: the session's own
    /// playout (default) or frames the host pushes with
    /// [`push_echo_reference`](Self::push_echo_reference).
    pub fn set_echo_reference(&mut self, source: EchoReference) {
        self.processing.set_reference_source(source);
        if source == EchoReference::Playout {
            self.processing.set_reference_attached(self.output_tap);
        } else {
            self.processing.set_reference_attached(false);
        }
    }

    /// Pushes far-end reference samples (mono, at the capture rate, about one 20 ms frame per
    /// tick) when the source is [`EchoReference::External`]: for instance the final game mix
    /// tapped after the bus tree. Ignored for the `Playout` source.
    pub fn push_echo_reference(&mut self, samples: &[f32]) {
        if self.processing.reference_source() == EchoReference::External {
            self.processing.push_reference(samples);
        }
    }

    // ----- observation ---------------------------------------------------------------------

    /// Per-speaker counters, ascending by sender.
    #[must_use]
    pub fn speaker_stats(&self) -> Vec<(u16, super::speaker::SpeakerStats)> {
        self.speakers.iter().map(|(id, s)| (*id, s.stats())).collect()
    }

    /// The remote speaker, if known.
    #[must_use]
    pub fn speaker(&self, sender: u16) -> Option<&VoiceSpeaker> {
        self.speakers.get(&sender)
    }

    /// Snapshot for `voice_chat.status`.
    #[must_use]
    pub fn status(&self) -> VoiceChatStatus {
        let state = self.tx.state();
        let effective = self.processing.effective(self.settings.deterministic);
        VoiceChatStatus {
            state: state.name(),
            error: if let TxState::Error(code) = state {
                Some(*code)
            } else {
                None
            },
            consent: self.user.consent,
            can_prompt: self.can_prompt,
            microphone: self.mic_phase.name(),
            device: self.device.as_ref().map(|d| d.name.clone()),
            codec: self.tx_codec.name(),
            bitrate_bps: self.bitrate_bps,
            mode: self.user.mode.name(),
            quality: self.settings.quality.name(),
            rate: self.settings.quality.sample_rate(),
            channels: self
                .settings
                .channels
                .iter()
                .map(|c| {
                    (
                        c.id,
                        c.name.clone(),
                        c.kind.name(),
                        self.joined.contains(&c.id),
                        self.active == Some(c.id),
                    )
                })
                .collect(),
            speakers: self
                .speakers
                .iter()
                .map(|(id, s)| SpeakerStatus {
                    sender: *id,
                    state: s.state().name(),
                    muted: s.muted,
                    blocked: self.is_sender_silenced(*id),
                    level_db: s.level_db(),
                })
                .collect(),
            loopback: self.loopback_since.is_some(),
            test_mode: self.test_source.is_some() || !self.fakes.is_empty(),
            effective_processing: effective,
            errors: self
                .errors
                .iter()
                .map(|(code, (count, last_frame))| StreamErrorRecord {
                    code,
                    count: *count,
                    last_frame: *last_frame,
                })
                .collect(),
        }
    }

    /// The configured quality (for the agent).
    #[must_use]
    pub fn quality(&self) -> VoiceQuality {
        self.settings.quality
    }
}

/// Counts one error code on `frame` in a session's error record.
fn record_error(errors: &mut BTreeMap<&'static str, (u64, u64)>, code: &'static str, frame: u64) {
    let entry = errors.entry(code).or_insert((0, frame));
    entry.0 = entry.0.saturating_add(1);
    entry.1 = frame;
}

/// Moves packets between a session and a transport: sends the queued outgoing packets and feeds
/// every polled packet to the session.
pub fn pump_transport(session: &mut VoiceChatSession, transport: &mut dyn VoiceChatTransport) {
    if let Some(quality) = transport.link_quality() {
        session.set_link_quality(quality);
    }
    for p in session.take_outgoing() {
        transport.send(&p);
    }
    while let Some(p) = transport.poll() {
        session.receive_packet(&p);
    }
}

/// The world resource holding the session behind a lock (codecs are not `Sync`).
#[derive(Resource)]
pub struct VoiceChatResource(pub Mutex<VoiceChatSession>);

impl VoiceChatResource {
    /// Locks the session for the engine's systems, recovering it from a poisoned lock (a panic in
    /// a codec or provider callback while it was held) with one warning, instead of every system
    /// silently doing nothing for the rest of the run (concurrency audit 11-03 L4).
    pub(crate) fn lock_session(&self) -> std::sync::MutexGuard<'_, VoiceChatSession> {
        self.0.lock().unwrap_or_else(|poisoned| {
            static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    target: "regia_audio",
                    "voice chat session lock was poisoned by a panic; continuing with its state"
                );
            }
            poisoned.into_inner()
        })
    }
}

/// Largest magnitude of a user gain setting, dB (a linear gain of 10^6 or 10^-6).
pub const MAX_USER_GAIN_DB: f32 = 120.0;

/// The linear gain of a user dB setting (`input_gain_db`, `output_gain_db`). The settings file is
/// user data: a non-finite value is unity and a finite one is clamped to ±[`MAX_USER_GAIN_DB`],
/// so the gain is always finite (a NaN `output_gain_db` used to push NaN samples for every
/// speaker into the playout; audit-gameplay Medium).
#[must_use]
pub fn user_gain(db: f32) -> f32 {
    if !db.is_finite() {
        return 1.0;
    }
    10f32.powf(db.clamp(-MAX_USER_GAIN_DB, MAX_USER_GAIN_DB) / 20.0)
}
