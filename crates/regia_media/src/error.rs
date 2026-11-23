// SPDX-License-Identifier: MPL-2.0
//! `MediaError`: every failure of the media layer, one variant per protocol code (specs/50 § 5.3).

use thiserror::Error;

/// A media failure. Each variant maps to exactly one lowercase protocol code ([`MediaError::code`]);
/// on the agent wire the code is `F0002` with `data.reason` = `video_` + the code.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MediaError {
    /// No player with that id or name.
    #[error("no video player `{0}`")]
    PlayerNotFound(String),
    /// The source asset or file does not exist.
    #[error("video source `{0}` not found")]
    SourceNotFound(String),
    /// The request is not valid in the player's current state.
    #[error("`{action}` is not valid in state {state}")]
    BadState {
        /// The refused action.
        action: String,
        /// The state the player was in.
        state: String,
    },
    /// The file is EBML/Matroska but malformed beyond recovery.
    #[error("invalid container: {0}")]
    ContainerInvalid(String),
    /// The file is a container family Regia Engine does not read (MP4, MOV, AVI, transport streams).
    #[error("unsupported container {0}: re-encode to AV1 in WebM")]
    ContainerUnsupported(String),
    /// A royalty-free codec without a decoder in this build, or an unknown codec id.
    #[error("codec `{codec}` is not supported by this build{hint}")]
    CodecUnsupported {
        /// The Matroska codec id.
        codec: String,
        /// Extra hint text (starts with a separator, or is empty).
        hint: String,
    },
    /// A patent-pooled codec, refused by the codec rule (specs/50 § 1).
    #[error("codec `{0}` is patent-pooled and never decoded: re-encode to AV1 in WebM")]
    CodecPatented(String),
    /// The requested track does not exist.
    #[error("track {0} not found")]
    TrackNotFound(u32),
    /// A rate outside `0.25..=4.0`, or a negative rate. `PlayerCore::set_rate(0.0)` pauses
    /// instead of returning this error. `video.create_player` still rejects `0.0` before a player
    /// exists (`video_rate_unsupported`).
    #[error("rate {0} is not supported (forward rates 0.25..=4.0)")]
    RateUnsupported(String),
    /// A frame seek past the last known frame.
    #[error("frame {0} is past the last frame")]
    SeekOutOfRange(u64),
    /// A configured limit (picture size, open players, allocation cap) was exceeded.
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    /// The decoder failed unrecoverably (too many corrupt packets, a caught panic, a native error).
    #[error("decode failed: {0}")]
    DecodeFailed(String),
    /// Reading the file failed.
    #[error("i/o failed: {0}")]
    IoFailed(String),
    /// The named audio bus is missing (a warning; playback falls back to master).
    #[error("audio bus `{0}` is missing; using master")]
    AudioBusMissing(String),
    /// A replacement material has no `video` texture parameter.
    #[error("material has no video texture slot")]
    MaterialMissingVideoSlot,
}

impl MediaError {
    /// Every protocol code, in the order of specs/50 § 5.3.
    pub const CODES: &'static [&'static str] = &[
        "player_not_found",
        "source_not_found",
        "bad_state",
        "container_invalid",
        "container_unsupported",
        "codec_unsupported",
        "codec_patented",
        "track_not_found",
        "rate_unsupported",
        "seek_out_of_range",
        "limit_exceeded",
        "decode_failed",
        "io_failed",
        "audio_bus_missing",
        "material_missing_video_slot",
    ];

    /// The lowercase protocol code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::PlayerNotFound(_) => "player_not_found",
            Self::SourceNotFound(_) => "source_not_found",
            Self::BadState { .. } => "bad_state",
            Self::ContainerInvalid(_) => "container_invalid",
            Self::ContainerUnsupported(_) => "container_unsupported",
            Self::CodecUnsupported { .. } => "codec_unsupported",
            Self::CodecPatented(_) => "codec_patented",
            Self::TrackNotFound(_) => "track_not_found",
            Self::RateUnsupported(_) => "rate_unsupported",
            Self::SeekOutOfRange(_) => "seek_out_of_range",
            Self::LimitExceeded(_) => "limit_exceeded",
            Self::DecodeFailed(_) => "decode_failed",
            Self::IoFailed(_) => "io_failed",
            Self::AudioBusMissing(_) => "audio_bus_missing",
            Self::MaterialMissingVideoSlot => "material_missing_video_slot",
        }
    }

    /// The fix hint `explain_error` shows with the code.
    pub fn hint(&self) -> &'static str {
        match self {
            Self::PlayerNotFound(_) => "list players with video.list",
            Self::SourceNotFound(_) => "check the path; media files live under Movies/ by convention",
            Self::BadState { .. } => "check video.status first; open the player before play",
            Self::ContainerInvalid(_) => "the file is damaged or truncated; re-export it",
            Self::ContainerUnsupported(_) | Self::CodecPatented(_) => {
                "re-encode to AV1 video with Opus or Vorbis audio in a .webm file"
            }
            Self::CodecUnsupported { .. } => {
                "use AV1 (video) and Opus or Vorbis (audio); build with the `video` feature for native decoders"
            }
            Self::TrackNotFound(_) => "video.probe lists the tracks",
            Self::RateUnsupported(_) => "use a forward rate in 0.25..=4.0; seek to scrub",
            Self::SeekOutOfRange(_) => "frame seeks need a frame below the clip's frame count",
            Self::LimitExceeded(_) => "use a smaller variant (variants in .video.ron) or close a player",
            Self::DecodeFailed(_) => "the stream is corrupt; re-encode it",
            Self::IoFailed(_) => "check the file is readable and not truncated",
            Self::AudioBusMissing(_) => "add the bus or set audio_bus in .video.ron",
            Self::MaterialMissingVideoSlot => "give the material a texture parameter of kind `video`",
        }
    }

    /// Shorthand for an I/O error.
    pub fn io(e: impl std::fmt::Display) -> Self {
        Self::IoFailed(e.to_string())
    }

    /// Shorthand for an invalid container.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::ContainerInvalid(msg.into())
    }

    /// Shorthand for a refused state transition.
    pub fn bad_state(action: &str, state: impl std::fmt::Display) -> Self {
        Self::BadState {
            action: action.to_string(),
            state: state.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_lowercase_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for code in MediaError::CODES {
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            );
            assert!(seen.insert(*code), "duplicate {code}");
        }
        assert_eq!(
            MediaError::CodecPatented("V_MPEG4/ISO/AVC".into()).code(),
            "codec_patented"
        );
        assert_eq!(
            MediaError::MaterialMissingVideoSlot.code(),
            "material_missing_video_slot"
        );
    }
}
