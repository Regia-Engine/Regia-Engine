// SPDX-License-Identifier: MIT-0
//! Arena audio: the two one-shot sound clips (gunshot, explosion) and their kind tag.
//!
//! The clips are tiny procedural samples (a decaying burst) so the sample needs no audio assets;
//! the weapon systems `play_sound` plays them spatially with
//! [`Audio::play_at`](regia_audio::Audio::play_at).

use regia_app::prelude::*;
use regia_audio::SoundClip;

/// The two combat sounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundKind {
    /// A rifle shot.
    Gunshot,
    /// An explosion.
    Explosion,
}

/// The prebuilt one-shot clips the arena plays.
#[derive(Resource)]
pub struct ArenaAudio {
    /// Rifle shot.
    pub gunshot: SoundClip,
    /// Explosion.
    pub explosion: SoundClip,
}

impl ArenaAudio {
    /// Builds the clips. Falls back to a silent clip if a sound cannot be constructed (never
    /// panics — audio is recoverable).
    pub fn build() -> Self {
        Self {
            gunshot: Self::gunshot_clip(),
            explosion: Self::explosion_clip(),
        }
    }

    /// A short, bright click: a few cycles of a decaying square wave.
    fn gunshot_clip() -> SoundClip {
        let samples: Vec<f32> = (0..480)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                let env = (-t * 200.0).exp();
                let wave = ((i / 8) % 2 == 0) as i32 as f32 * 2.0 - 1.0;
                wave * env
            })
            .collect();
        SoundClip::new(samples, 48_000).unwrap_or_else(|_| {
            SoundClip::new(vec![0.0; 16], 48_000).expect("16 samples at 48 kHz is always valid")
        })
    }

    /// A longer, lower rumble: noise with a slow decay.
    fn explosion_clip() -> SoundClip {
        // A tiny deterministic pseudo-noise (no `rand` dependency).
        let samples: Vec<f32> = (0..4800)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                let env = (-t * 8.0).exp();
                let x = (i as u32).wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
                let noise = ((x >> 9) as f32 / (u32::MAX >> 9) as f32) * 2.0 - 1.0;
                noise * env
            })
            .collect();
        SoundClip::new(samples, 48_000).unwrap_or_else(|_| {
            SoundClip::new(vec![0.0; 16], 48_000).expect("16 samples at 48 kHz is always valid")
        })
    }
}

impl Default for ArenaAudio {
    fn default() -> Self {
        Self::build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_are_non_empty() {
        let audio = ArenaAudio::build();
        assert!(!audio.gunshot.is_empty());
        assert!(!audio.explosion.is_empty());
        assert!(audio.gunshot.sample_rate() == 48_000);
    }
}
