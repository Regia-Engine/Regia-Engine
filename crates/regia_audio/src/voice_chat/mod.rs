// SPDX-License-Identifier: MPL-2.0
//! Voice chat: codec, packets, jitter buffer, channels, routing, moderation and the packet-loss
//! simulator (`docs/specs/59-voice-chat.md`, ADR-1647).
//!
//! The feature is always called `voice_chat` in code (the mixer's "voices" and the older capture
//! baseline in [`crate::AudioCapture`] are different things). Everything here is plain Rust over
//! frame counters: no clock, no device, no network crate. The seam to a network is
//! [`VoiceChatTransport`] (`regia_net::voice` implements it, feature `voice`); the always-built
//! codec is [`PcmCodec`] and Opus arrives through [`VoiceCodecProvider`] (`regia_media`, feature
//! `voice-opus`, ADR-1911). The microphone is an [`InputDeviceOpener`]: [`CpalInputOpener`] on a
//! real device, [`FakeMicrophone`] in tests.
//!
//! # Example
//!
//! A packet round trip through the always-built PCM codec:
//!
//! ```
//! use regia_audio::voice_chat::{PcmCodec, VoiceCodec, VoiceCodecId, VoiceFlags, VoicePacket, VoiceQuality};
//!
//! let quality = VoiceQuality::Narrow;
//! let mut codec = PcmCodec::new(quality);
//! let frame = vec![0.25f32; quality.frame_samples()];
//! let mut payload = vec![0u8; 1 + 2 * frame.len()];
//! let n = codec.encode(&frame, &mut payload).expect("encodes");
//! let packet = VoicePacket {
//!     flags: VoiceFlags::default(),
//!     sender: 7,
//!     channel: 2,
//!     seq: 1,
//!     timestamp: 960,
//!     quality,
//!     codec: VoiceCodecId::Pcm,
//!     payload: &payload[..n],
//! };
//! let mut wire = Vec::new();
//! packet.write(&mut wire);
//! assert_eq!(VoicePacket::parse(&wire).expect("parses"), packet);
//! ```

mod aec;
mod agc;
mod capture;
mod codec;
mod denoise;
#[cfg(test)]
mod dsp_fixtures;
mod emitter;
mod error;
mod jitter;
mod mic;
mod moderation;
mod packet;
mod plugin;
mod processing;
mod rng;
mod routing;
mod session;
mod settings;
mod simulator;
mod source;
mod speaker;
mod timescale;
mod transmit;

pub use aec::{AEC_DOUBLE_TALK_STEP_SCALE, AEC_STEP, AEC_TAIL_MS, AecStats, EchoCanceller};
pub use agc::{
    AGC_ATTACK_DB, AGC_GATE_DB, AGC_GATE_MARGIN_DB, AGC_LIMIT, AGC_MAX_GAIN_DB, AGC_MIN_GAIN_DB,
    AGC_RELEASE_DB, AGC_TARGET_DB, AgcStats, AutoGain,
};
pub use capture::{CaptureConverter, Framer, HighPass, Resampler, downmix_to_mono};
pub use codec::{BuiltinCodecs, PcmCodec, SharedCodecProvider, VoiceCodec, VoiceCodecProvider, new_codec};
pub use denoise::{
    NS_BIAS, NS_GAIN_FLOOR_DB, NS_MIN_WINDOWS, NS_SUBWINDOW_HOPS, NoiseSuppressor, NoiseSuppressorStats,
};
pub(crate) use emitter::pan_gains;
pub use emitter::{
    FLAT_UNRESOLVED_DB, SPATIAL_CROSSFADE_FRAMES, SpeakerMixer, VoiceChatEmitter, VoiceChatOutput,
    VoiceChatOutputStats, VoiceChatSpeaking, VoicePlacement, voice_emitter_params, voice_placement,
};
pub use error::{ERROR_CODES, VoiceChatError};
pub use jitter::{
    FRAME_MS, InsertOutcome, JITTER_WARMUP, JitterBuffer, JitterSettings, JitterStats, MAX_CONCEALED,
    Playout, WINDOW_FRAMES, seq_diff, seq_newer,
};
pub use mic::{
    CpalInputOpener, FakeMicrophone, InputDevice, InputDeviceOpener, MicPermission, classify_capture_error,
};
pub use moderation::{
    ACCOUNT_LIST_CAP, AccountEntry, AccountList, REPORT_BUFFER_MAX_SECONDS, ReportRing, VoiceReport,
    VoiceReportSink,
};
pub use packet::{
    FRAME_TIMESTAMP_STEP, HEADER_LEN, MAX_PAYLOAD, MAX_PCM_PAYLOAD, MULTI_FRAMES, MULTI_REDUNDANT,
    MultiFrames, PACKET_VERSION, TIMESTAMP_RATE, VoiceCodecId, VoiceFlags, VoicePacket, VoiceQuality,
    parse_multi, write_multi,
};
pub use plugin::{VOICE_TALK_ACTION, VoiceChatCodecs, VoiceChatPlugin, VoiceChatTalk};
pub use processing::{
    EchoReference, HIGH_PASS_HZ, ProcessingChain, ProcessingStats, REFERENCE_MAX_FRAMES, STAGE_NAMES,
    StageStats, mix_into,
};
pub use rng::Pcg32;
pub use routing::{
    Candidate, LISTENER_CAP_EXTRA, Member, MembershipTable, PROXIMITY_CACHE_MS, PROXIMITY_SLACK_M,
    RANK_STAY_IN_MARGIN_DB, rank_speakers, rank_speakers_sticky,
};
pub use session::{
    ACTIVITY_DEBOUNCE_FRAMES, MAX_BOUND_SENDERS, MAX_PENDING_DECODED_FRAMES, MAX_REMOTE_SPEAKERS,
    MAX_USER_GAIN_DB, SessionStats, SpeakerStatus, StreamErrorRecord, VoiceChatEvent, VoiceChatResource,
    VoiceChatSession, VoiceChatStatus, VoiceControl, VoiceLinkQuality, pump_transport, user_gain,
};
pub use settings::{
    CONFIG_FORMAT_VERSION, LOOPBACK_MAX_FRAMES, LOOPBACK_SENDER, NoiseSuppression, ProcessingSettings,
    VoiceChannel, VoiceChannelId, VoiceChannelKind, VoiceChatSettings, VoiceChatUserSettings,
    voice_chat_config_migration_sources, voice_chat_config_schema_fingerprint,
};
pub use simulator::{SimulatorSettings, SimulatorStats, VoiceChatSimulator};
pub use source::TestSource;
pub use speaker::{SPEAKER_TIMEOUT_FRAMES, SpeakerState, SpeakerStats, VoiceSpeaker};
pub use timescale::{lengthen, shorten, tail_period};
pub use transmit::{
    ADAPTIVE_OFF_MARGIN_DB, ADAPTIVE_ON_MARGIN_DB, FrameVad, FrameVadSettings, HANGOVER_FRAMES,
    NOISE_FLOOR_FRAMES, PTT_GRACE_FRAMES, RETRY_FRAMES, TalkGate, TransmitMachine, TxFrame, TxState,
    VoiceMode, frame_rms_db,
};

/// The seam between the voice code and a network (spec § 3). Implemented by
/// [`VoiceChatSimulator`] for tests and loopback, by the game, or by `regia_net::voice`.
/// Voice packets are unreliable and unfragmented.
pub trait VoiceChatTransport {
    /// Sends one serialized packet.
    fn send(&mut self, packet: &[u8]);
    /// Takes the next received packet, if any.
    fn poll(&mut self) -> Option<Vec<u8>>;
    /// What the transport measured on its link, for the adaptive bit rate. `None` when it does
    /// not measure.
    fn link_quality(&self) -> Option<VoiceLinkQuality> {
        None
    }
}
