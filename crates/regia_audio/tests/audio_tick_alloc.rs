// SPDX-License-Identifier: MPL-2.0
//! A steady mix of real looping voices ticks without allocating.
//!
//! Thirty-two voices sit under the desktop voice limit, so nothing virtualises or revives.
//! Ducking stays idle (no announcer, no voice chat). Gains do not move.

#![allow(unsafe_code)]

include!("support/alloc_count.rs");

use regia_audio::{Audio, AudioSettings, LoopRegion, PlaybackParams, SoundAsset, SoundClip};

fn headless() -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    })
}

fn tone() -> SoundAsset {
    let len = 2_400;
    let samples: Vec<f32> = (0..len)
        .map(|i| ((i as f32 / 48_000.0) * 440.0 * std::f32::consts::TAU).sin() * 0.2)
        .collect();
    let clip = match SoundClip::new(samples, 48_000) {
        Ok(clip) => clip,
        Err(error) => panic!("tone clip: {error}"),
    };
    SoundAsset::from_clip(&clip)
}

#[test]
fn steady_real_voices_tick_without_allocating() {
    let mut audio = headless();
    let asset = tone();
    let params = PlaybackParams {
        loop_region: Some(LoopRegion {
            start_secs: 0.0,
            end_secs: None,
        }),
        ..PlaybackParams::default()
    };
    for _ in 0..32 {
        match audio.start_asset(&asset, params.clone()) {
            Ok(_) => {}
            Err(error) => panic!("start voice: {error}"),
        }
    }
    audio.tick(1.0 / 60.0);
    let before = allocations();
    for _ in 0..60 {
        audio.tick(1.0 / 60.0);
    }
    let allocs = allocations() - before;
    assert_eq!(allocs, 0, "60 steady ticks allocated {allocs} times");
}
