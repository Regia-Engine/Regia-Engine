// SPDX-License-Identifier: MPL-2.0
//! Shared helpers for the crate's own tests.

use crate::clip::SoundClip;
use crate::sound::SoundAsset;

/// A short valid asset (8 samples at 48 kHz).
pub(crate) fn asset() -> SoundAsset {
    let clip = SoundClip::new(vec![0.2; 8], 48_000).expect("8 samples at 48 kHz are a valid clip");
    SoundAsset::from_clip(&clip)
}

/// A looping tone asset: `seconds` of a sine at `hz`, amplitude `amp`, 48 kHz mono.
#[allow(dead_code)] // used by src/tests.rs and future integration helpers
pub(crate) fn tone_asset(hz: f32, seconds: f64, amp: f32) -> SoundAsset {
    let len = (seconds * 48_000.0) as usize;
    let samples: Vec<f32> = (0..len)
        .map(|i| {
            let t = i as f32 / 48_000.0;
            (t * hz * std::f32::consts::TAU).sin() * amp
        })
        .collect();
    let clip = SoundClip::new(samples, 48_000).expect("a sine is a valid clip");
    SoundAsset::from_clip(&clip)
}
