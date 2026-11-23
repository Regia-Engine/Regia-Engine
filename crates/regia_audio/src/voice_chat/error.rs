// SPDX-License-Identifier: MPL-2.0
//! Errors of the voice chat feature (`docs/specs/59-voice-chat.md` § 5).

use thiserror::Error;

/// Errors from the voice chat codec, packet parser, channels and moderation.
///
/// [`VoiceChatError::code`] gives the lowercase agent code for each variant.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VoiceChatError {
    /// The microphone needs the user's consent first.
    #[error("microphone consent has not been given")]
    ConsentRequired,
    /// The OS or the user denied microphone access.
    #[error("microphone permission was denied")]
    PermissionDenied,
    /// No input device is available.
    #[error("no input device is available")]
    NoInputDevice,
    /// The input device is in use by something else.
    #[error("the input device is busy")]
    DeviceBusy,
    /// The device delivers a sample format or rate that cannot be converted.
    #[error("unsupported audio format: {0}")]
    UnsupportedFormat(String),
    /// The codec failed to encode or decode.
    #[error("codec error: {0}")]
    Codec(String),
    /// A packet could not be parsed.
    #[error("bad voice packet: {0}")]
    BadPacket(String),
    /// The channel id is not defined.
    #[error("unknown voice channel {0}")]
    UnknownChannel(u16),
    /// The local player has not joined the channel.
    #[error("not joined to voice channel {0}")]
    NotInChannel(u16),
    /// The sender id has no speaker.
    #[error("unknown voice speaker {0}")]
    UnknownSpeaker(u16),
    /// A request parameter is out of range.
    #[error("invalid parameters: {0}")]
    InvalidParams(String),
    /// An optional stage or codec is not compiled in.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The server muted this sender.
    #[error("the server muted this sender")]
    ServerMuted,
    /// The sender exceeded the transmit rate limit.
    #[error("voice packet rate limit exceeded")]
    RateLimited,
    /// The call is only valid in the developer test mode.
    #[error("only available in test mode")]
    TestModeOnly,
}

impl VoiceChatError {
    /// The lowercase agent error code (`data.reason` of an `F0002`), spec 59 § 5.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConsentRequired => "consent_required",
            Self::PermissionDenied => "permission_denied",
            Self::NoInputDevice => "no_input_device",
            Self::DeviceBusy => "device_busy",
            Self::UnsupportedFormat(_) => "unsupported_format",
            Self::Codec(_) => "codec_error",
            Self::BadPacket(_) => "bad_packet",
            Self::UnknownChannel(_) => "unknown_channel",
            Self::NotInChannel(_) => "not_in_channel",
            Self::UnknownSpeaker(_) => "unknown_speaker",
            Self::InvalidParams(_) => "invalid_params",
            Self::Unsupported(_) => "unsupported",
            Self::ServerMuted => "server_muted",
            Self::RateLimited => "rate_limited",
            Self::TestModeOnly => "test_mode_only",
        }
    }
}

/// Every agent error code of spec 59 § 5, sorted.
pub const ERROR_CODES: [&str; 15] = [
    "bad_packet",
    "codec_error",
    "consent_required",
    "device_busy",
    "invalid_params",
    "no_input_device",
    "not_in_channel",
    "permission_denied",
    "rate_limited",
    "server_muted",
    "test_mode_only",
    "unknown_channel",
    "unknown_speaker",
    "unsupported",
    "unsupported_format",
];
