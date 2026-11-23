// SPDX-License-Identifier: MPL-2.0
//! `.ambience.ron` v1 through the offline mixer (ADR-2721): a seeded file renders the same
//! samples twice, another seed renders others, and a v1 file round-trips.

use std::collections::HashMap;

use regia_audio::{AmbienceDef, AmbienceInputs, Audio, AudioSettings, BusLayout, SoundAsset, SoundClip};
use regia_core::math::Vec3;

const RATE: u32 = 48_000;
const EXAMPLE: &str = include_str!("../../../docs/formats/glade.ambience.ron");

fn tone(hz: f32, secs: f32, amp: f32) -> SoundAsset {
    let samples: Vec<f32> = (0..(secs * RATE as f32) as usize)
        .map(|i| (std::f32::consts::TAU * hz * i as f32 / RATE as f32).sin() * amp)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("tone"))
}

/// Every sound the example names, as distinct tones.
fn sounds() -> HashMap<String, SoundAsset> {
    let names = [
        ("amb/air_loop", 110.0, 1.0),
        ("amb/wind_loop", 170.0, 1.0),
        ("amb/rain_loop", 260.0, 1.0),
        ("amb/bird_a", 2_200.0, 0.3),
        ("amb/bird_b", 2_900.0, 0.25),
        ("amb/bird_c", 3_400.0, 0.2),
        ("amb/woodpecker", 900.0, 0.15),
    ];
    names
        .into_iter()
        .map(|(name, hz, secs)| (name.to_string(), tone(hz, secs, 0.4)))
        .collect()
}

/// Renders `def` for `ticks` × 10 ms with a breeze.
fn render(def: &AmbienceDef, ticks: usize) -> (Vec<f32>, u64) {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        buses: BusLayout::default_tree(),
        ..AudioSettings::default()
    });
    let library = sounds();
    let resolve = |name: &str| library.get(name).cloned();
    audio.play_ambience(def.clone()).expect("play");
    let inputs = AmbienceInputs {
        wind_m_per_s: 4.0,
        mean_wind_m_per_s: 3.0,
        ..AmbienceInputs::default()
    };
    let mut out = Vec::new();
    for _ in 0..ticks {
        audio.step_ambience_files(0.01, &inputs, Vec3::ZERO, &resolve);
        audio.tick(0.01);
        out.extend(audio.render_offline(480).expect("offline backend"));
    }
    let played = audio.ambience_files().first().map_or(0, |info| info.calls_played);
    (out, played)
}

#[test]
fn a_seeded_ambience_renders_the_same_samples_twice() {
    let def = AmbienceDef::from_ron(EXAMPLE).expect("the docs example");
    let (a, calls) = render(&def, 1_500);
    assert!(calls >= 3, "{calls} calls in 15 s");
    let (b, _) = render(&def, 1_500);
    assert_eq!(a.len(), b.len());
    assert!(
        a.iter().zip(&b).all(|(x, y)| x.to_bits() == y.to_bits()),
        "two runs differ"
    );
    assert!(a.iter().any(|s| s.abs() > 1e-4), "something is heard");
    let mut other = def.clone();
    other.seed ^= 0xA5A5;
    let (c, _) = render(&other, 1_500);
    assert!(
        a.iter().zip(&c).any(|(x, y)| x.to_bits() != y.to_bits()),
        "another seed sounds different"
    );
}

#[test]
fn a_v1_file_round_trips_and_stops_cleanly() {
    let def = AmbienceDef::from_ron(EXAMPLE).expect("the docs example");
    let text = def.to_ron().expect("serialize");
    assert_eq!(AmbienceDef::from_ron(&text).expect("reparse"), def);
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    audio.play_ambience(def.clone()).expect("play");
    assert_eq!(audio.ambience_files().len(), 1);
    // Playing it again replaces it.
    audio.play_ambience(def).expect("again");
    assert_eq!(audio.ambience_files().len(), 1);
    assert!(audio.stop_ambience("glade"));
    assert!(!audio.stop_ambience("glade"));
    assert!(audio.ambience_files().is_empty());
}
