// SPDX-License-Identifier: MPL-2.0
//! The cooked form of a sound source (`docs/specs/95-loudness-importers.md` § 2.3, ADR-2788): the
//! source file's bytes, kept as authored (`.wav`, `.ogg`, `.flac`, `.mp3`), next to the
//! [`SoundLoudness`] record the cook measured. A cook writes one per sound, so a pak carries the audio and its
//! loudness gain in the same entry and a packaged game plays sounds with no loose file
//! ([`SoundAssets::add_packed`](crate::SoundAssets::add_packed)).
//!
//! # Payload layout, version 1 (little-endian)
//!
//! | Offset | Field |
//! |---|---|
//! | 0 | magic `b"FSND"` |
//! | 4 | `version: u32` = [`COOKED_SOUND_VERSION`] |
//! | 8 | `record_len: u32`: bytes of the loudness record |
//! | 12 | the [`SoundLoudness`] record, RON text, `record_len` bytes |
//! | 12 + `record_len` | the source file's bytes, to the end (never empty) |
//!
//! The cooked file is a cache, not a project file: a version this runtime does not read is an
//! error ([`CookedSoundError::UnsupportedVersion`]) and a cook writes it again.

use thiserror::Error;

use crate::loudness_table::SoundLoudness;

/// Type path (and so the cooked header's type) of a cooked sound.
pub const COOKED_SOUND_TYPE: &str = "regia_audio::CookedSound";

/// Magic bytes at the start of a cooked sound payload.
pub const COOKED_SOUND_MAGIC: [u8; 4] = *b"FSND";

/// Current cooked sound layout version.
pub const COOKED_SOUND_VERSION: u32 = 1;

/// Fixed bytes before the record text.
const HEADER: usize = 12;

/// Why a cooked sound payload did not read. `code()` is the agent and log code.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CookedSoundError {
    /// Not a cooked sound, or too short for its header.
    #[error("not a cooked sound (bad magic or truncated header)")]
    BadMagic,
    /// A layout version this runtime does not read.
    #[error("cooked sound layout version {found} is not supported (this runtime reads {supported})")]
    UnsupportedVersion {
        /// Version in the payload.
        found: u32,
        /// Version this runtime reads.
        supported: u32,
    },
    /// The header's sizes do not match the payload, the record is not valid, or there is no audio.
    #[error("cooked sound is malformed: {0}")]
    Malformed(String),
}

impl CookedSoundError {
    /// The stable code: `cooked_sound_bad_magic`, `cooked_sound_version` or `cooked_sound_malformed`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadMagic => "cooked_sound_bad_magic",
            Self::UnsupportedVersion { .. } => "cooked_sound_version",
            Self::Malformed(_) => "cooked_sound_malformed",
        }
    }
}

/// A parsed cooked sound: the loudness record and the source file's bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct CookedSound {
    /// What the cook measured and the gain its policy gave the sound.
    pub loudness: SoundLoudness,
    /// The source file as authored.
    pub data: Vec<u8>,
}

impl CookedSound {
    /// Encodes the payload.
    ///
    /// # Errors
    ///
    /// [`CookedSoundError::Malformed`] for empty audio bytes or a record that does not serialize.
    pub fn to_bytes(&self) -> Result<Vec<u8>, CookedSoundError> {
        if self.data.is_empty() {
            return Err(CookedSoundError::Malformed("no audio bytes".to_owned()));
        }
        let record = ron::ser::to_string(&self.loudness)
            .map_err(|e| CookedSoundError::Malformed(format!("record: {e}")))?;
        let len = u32::try_from(record.len())
            .map_err(|_| CookedSoundError::Malformed("record too long".to_owned()))?;
        let mut out = Vec::with_capacity(HEADER + record.len() + self.data.len());
        out.extend_from_slice(&COOKED_SOUND_MAGIC);
        out.extend_from_slice(&COOKED_SOUND_VERSION.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(record.as_bytes());
        out.extend_from_slice(&self.data);
        Ok(out)
    }

    /// Parses and validates a payload.
    ///
    /// # Errors
    ///
    /// [`CookedSoundError`] for a bad magic, an unknown version, sizes that do not fit the payload
    /// or a record that is not valid text or RON. Every truncation is an error, never a panic.
    pub fn from_bytes(payload: &[u8]) -> Result<Self, CookedSoundError> {
        if payload.len() < HEADER || payload[..4] != COOKED_SOUND_MAGIC {
            return Err(CookedSoundError::BadMagic);
        }
        let word =
            |at: usize| u32::from_le_bytes([payload[at], payload[at + 1], payload[at + 2], payload[at + 3]]);
        let version = word(4);
        if version != COOKED_SOUND_VERSION {
            return Err(CookedSoundError::UnsupportedVersion {
                found: version,
                supported: COOKED_SOUND_VERSION,
            });
        }
        let record_len = word(8) as usize;
        let data_at = HEADER
            .checked_add(record_len)
            .filter(|&end| end < payload.len())
            .ok_or_else(|| CookedSoundError::Malformed("sizes run past the payload".to_owned()))?;
        let text = std::str::from_utf8(&payload[HEADER..data_at])
            .map_err(|_| CookedSoundError::Malformed("record is not text".to_owned()))?;
        let loudness =
            ron::from_str(text).map_err(|e| CookedSoundError::Malformed(format!("record: {e}")))?;
        Ok(Self {
            loudness,
            data: payload[data_at..].to_vec(),
        })
    }
}

/// The shape of the cooked payload, for the format registry (`cargo xtask check-formats`).
#[must_use]
pub fn cooked_sound_schema_fingerprint() -> String {
    "CookedSound{magic:FSND,version:u32le,record_len:u32le,record:ron(SoundLoudness),data:bytes}".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> SoundLoudness {
        let samples: Vec<f32> = (0..9600 * 2)
            .map(|i| ((i / 2) as f32 * 0.05).sin() * 0.25)
            .collect();
        SoundLoudness::measure_interleaved_stereo(&samples, 48_000).expect("measure")
    }

    #[test]
    fn round_trips_and_every_truncation_errors() {
        let sound = CookedSound {
            loudness: record(),
            data: b"OggS-not-really".to_vec(),
        };
        let bytes = sound.to_bytes().expect("encode");
        assert_eq!(CookedSound::from_bytes(&bytes).expect("decode"), sound);
        // Cut anywhere before the first audio byte: an error. (Cut audio is still audio; the decoder judges it.)
        let audio_at = bytes.len() - sound.data.len();
        for len in 0..=audio_at {
            assert!(
                CookedSound::from_bytes(&bytes[..len]).is_err(),
                "truncated to {len}"
            );
        }
        assert_eq!(
            CookedSound::from_bytes(&bytes[..5]).expect_err("short").code(),
            "cooked_sound_bad_magic"
        );
        let mut newer = bytes.clone();
        newer[4] = 2;
        assert_eq!(
            CookedSound::from_bytes(&newer).expect_err("newer").code(),
            "cooked_sound_version"
        );
        let mut hostile = bytes;
        hostile[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            CookedSound::from_bytes(&hostile).expect_err("huge").code(),
            "cooked_sound_malformed"
        );
        assert!(
            CookedSound {
                loudness: record(),
                data: Vec::new()
            }
            .to_bytes()
            .is_err()
        );
    }
}
