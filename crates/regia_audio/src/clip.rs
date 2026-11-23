// SPDX-License-Identifier: MPL-2.0
//! Sound clips: short PCM audio owned in memory (`docs/specs/16-audio.md` § 2, static sounds).
//!
//! This slice constructs clips in code; loading clips from assets arrives with the asset
//! integration.

use std::{fmt, sync::Arc};

/// A short clip of mono PCM samples: `f32` frames at a sample rate, owned in memory.
///
/// Cloning is cheap (samples are shared through an `Arc`), so gameplay can keep a clip around and
/// play it as a one-shot whenever something happens.
#[derive(Debug, Clone, PartialEq)]
pub struct SoundClip {
    pub(crate) samples: Arc<[f32]>,
    pub(crate) sample_rate: u32,
}

/// Errors from [`SoundClip::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundClipError {
    /// The clip has zero samples: nothing to play.
    Empty,
    /// The sample rate is zero, which no output can use.
    ZeroSampleRate,
}

impl fmt::Display for SoundClipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("sound clip has zero samples"),
            Self::ZeroSampleRate => f.write_str("sound clip sample rate is zero"),
        }
    }
}

impl std::error::Error for SoundClipError {}

impl SoundClip {
    /// Creates a clip from mono samples. A zero-length clip and a zero sample rate are rejected
    /// with an error instead of a panic.
    ///
    /// ```
    /// # use regia_audio::SoundClip;
    /// let clip = SoundClip::new(vec![0.25, -0.25], 48_000).expect("a valid clip");
    /// assert_eq!(clip.len(), 2);
    /// assert_eq!(clip.sample_rate(), 48_000);
    /// ```
    pub fn new(samples: impl Into<Arc<[f32]>>, sample_rate: u32) -> Result<Self, SoundClipError> {
        let samples = samples.into();
        if samples.is_empty() {
            return Err(SoundClipError::Empty);
        }
        if sample_rate == 0 {
            return Err(SoundClipError::ZeroSampleRate);
        }
        Ok(Self { samples, sample_rate })
    }

    /// The mono samples of the clip.
    #[must_use]
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// The number of samples in the clip.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// True when the clip has no samples. A clip built through [`SoundClip::new`] is never empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The sample rate of the clip, in Hz.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_zero_rate_with_errors() {
        assert_eq!(
            SoundClip::new(Vec::<f32>::new(), 48_000),
            Err(SoundClipError::Empty)
        );
        assert_eq!(SoundClip::new(vec![0.0], 0), Err(SoundClipError::ZeroSampleRate));
    }

    #[test]
    fn exposes_samples_and_rate() {
        let clip = SoundClip::new(vec![0.5, -0.5, 1.0], 44_100).expect("a valid clip");
        assert_eq!(clip.len(), 3);
        assert!(!clip.is_empty());
        assert_eq!(clip.sample_rate(), 44_100);
        assert_eq!(clip.samples(), &[0.5, -0.5, 1.0]);
    }
}
