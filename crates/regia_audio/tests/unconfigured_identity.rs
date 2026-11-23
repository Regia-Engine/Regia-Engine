// SPDX-License-Identifier: MPL-2.0
//! An unconfigured project mixes bit for bit as before convolution existed (ADR-2721): the
//! hidden zone-IR send bus and its silent sends add nothing. The pin below was computed by this
//! same file on the tree before the convolution lane (`a27626fc8`, the two dependency lanes
//! merged), on Linux x86_64; this file uses only API that tree has.

use regia_audio::{
    Audio, AudioSettings, BusLayout, LoopRegion, PlaybackParams, ReverbMix, SoundAsset, SoundClip,
};

const RATE: u32 = 48_000;

fn tone(hz: f32, secs: f32, amp: f32) -> SoundAsset {
    let samples: Vec<f32> = (0..(secs * RATE as f32) as usize)
        .map(|i| (std::f32::consts::TAU * hz * i as f32 / RATE as f32).sin() * amp)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("tone"))
}

fn looping(bus: &str) -> PlaybackParams {
    PlaybackParams {
        bus: Some(bus.into()),
        loop_region: Some(LoopRegion {
            start_secs: 0.0,
            end_secs: None,
        }),
        ..PlaybackParams::default()
    }
}

/// FNV-1a over the sample bits.
fn hash(samples: &[f32]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for s in samples {
        for b in s.to_bits().to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

fn render_default_mix() -> Vec<f32> {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        buses: BusLayout::default_tree(),
        ..AudioSettings::default()
    });
    for (bus, hz) in [
        ("World", 220.0),
        ("Ambience", 330.0),
        ("Music", 440.0),
        ("Ui", 550.0),
    ] {
        audio
            .start_asset(&tone(hz, 1.0, 0.2), looping(bus))
            .expect("play");
    }
    let mut out = Vec::new();
    for tick in 0..300 {
        // A reverb zone raises the sends for a while, then the listener leaves it.
        if tick == 50 {
            audio.set_reverb_mix(Some(ReverbMix {
                weight: 1.0,
                send_db: -6.0,
                ..ReverbMix::DRY
            }));
        }
        if tick == 200 {
            audio.set_reverb_mix(None);
        }
        audio.tick(0.01);
        out.extend(audio.render_offline(480).expect("offline backend"));
    }
    out
}

/// Every platform: the default mix renders deterministically (two runs in one process give the
/// same bits) at the expected length. This is the smoke half of the check — the bit-exact pin
/// below runs on Linux x86_64 only, the platform the pin was computed on (see the module docs).
#[test]
fn the_default_mix_is_deterministic() {
    let samples = render_default_mix();
    assert_eq!(samples.len(), 300 * 480 * 2);
    assert_eq!(hash(&samples), hash(&render_default_mix()));
}

/// The bit-exact pin (Linux x86_64 only): the unconfigured mix must not change. Other platforms
/// run `the_default_mix_is_deterministic` above; a self-equality assert cannot fail, so it is
/// not passed off as the pin there (rc1-rev finding 10). Per-platform pins need a render on
/// each platform first.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn the_default_mix_is_bit_identical_to_the_tree_before_convolution() {
    let got = hash(&render_default_mix());
    println!("default mix hash {got:#018x}");
    assert_eq!(got, PIN, "the default mix changed: {got:#018x}");
}

/// The hash the pre-convolution tree renders (see the module docs).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PIN: u64 = 0x6336_cf8e_f44d_48c5;
