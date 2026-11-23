// SPDX-License-Identifier: MPL-2.0
//! Errors of the audio crate, one enum per boundary (`docs/04-conventions.md` § 4).

use thiserror::Error;

/// Errors from loading, routing or starting audio.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AudioError {
    /// The clip has no samples, so there is nothing to play.
    #[error("cannot play an empty (zero-sample) clip")]
    EmptyClip,
    /// A sound file could not be opened or decoded. Carries the path and the decoder message.
    #[error("audio file `{path}` failed to load: {message}")]
    LoadFailed {
        /// The path or label of the sound that failed.
        path: String,
        /// The underlying decoder error message.
        message: String,
    },
    /// A sound decoded whole into memory (a static sound, a stream decoded for an offline render,
    /// a loudness measurement) would go past its decode limits (`SoundDecodeLimits`: 30 minutes or
    /// 1 GiB of frames by default, ADR-4922). Nothing past the limit was decoded. Stream long
    /// audio instead.
    #[error(
        "audio file `{path}` decodes to more than {max_frames} frames at {sample_rate} Hz, past the \
         decode limit (stream long audio instead)"
    )]
    DecodeLimit {
        /// The path or label of the sound.
        path: String,
        /// The most frames the limits allow at this sample rate.
        max_frames: u64,
        /// The sound's sample rate, Hz.
        sample_rate: u32,
    },
    /// A sound file declares a sample rate above `MAX_SOUND_SAMPLE_RATE` (384 kHz). Refused
    /// before any frame is decoded.
    #[error("audio file `{path}` declares a {sample_rate} Hz sample rate, above the {max} Hz maximum")]
    SampleRateTooHigh {
        /// The path or label of the sound.
        path: String,
        /// The declared sample rate, Hz.
        sample_rate: u32,
        /// The maximum (`MAX_SOUND_SAMPLE_RATE`).
        max: u32,
    },
    /// The backend refused to start a sound (resource limits, dead device, decoder error).
    #[error("the audio backend rejected the sound: {0}")]
    PlayFailed(String),
    /// A bus name was used that the layout does not define.
    #[error("audio bus `{0}` does not exist")]
    UnknownBus(String),
    /// A snapshot name was pushed that was never registered.
    #[error("audio snapshot `{0}` is not registered")]
    UnknownSnapshot(String),
    /// A music track or cue was referenced but not registered with the audio system.
    #[error("audio cue or track `{0}` is not registered")]
    UnknownEntry(String),
    /// A definition or request is invalid; the message is the whole diagnosis (an empty cue, a
    /// patch layer without a sound, a missing patch parameter, a music track without stems).
    #[error("{0}")]
    Invalid(String),
    /// An audio patch asset with an unsupported version was loaded (frozen format: v1).
    #[error("audio patch format version {0} is not supported (this engine reads version 1)")]
    UnsupportedPatchVersion(u32),
    /// A patch migration step failed or is missing.
    #[error("audio patch migration failed: {0}")]
    Migration(String),
    /// The microphone could not be opened or its stream died.
    #[error("voice capture error: {0}")]
    Capture(String),
    /// The operation needs the offline (headless) backend, but a real device is in use.
    #[error("this operation is only available on the offline audio backend")]
    NeedsOffline,
}

impl AudioError {
    /// Builds a [`AudioError::LoadFailed`] without naming the fields at every call site.
    pub(crate) fn load(path: impl Into<String>, message: impl std::fmt::Display) -> Self {
        Self::LoadFailed {
            path: path.into(),
            message: message.to_string(),
        }
    }
}
