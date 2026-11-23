// SPDX-License-Identifier: MPL-2.0
//! A playing patch with constant parameters ticks without allocating, and still changes pitch
//! when a parameter moves.

#![allow(unsafe_code)]

include!("support/alloc_count.rs");

use std::collections::HashMap;

use regia_audio::{
    Audio, AudioPatch, AudioPatchDef, AudioSettings, PatchCurveDef, PatchKeyDef, PatchLayerDef,
    PatchParamDef, SoundAsset, SoundClip,
};

fn headless() -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    })
}

fn looping_tone(hz: f32) -> SoundAsset {
    let len = 24_000;
    let samples: Vec<f32> = (0..len)
        .map(|i| ((i as f32 / 48_000.0) * hz * std::f32::consts::TAU).sin() * 0.3)
        .collect();
    let clip = match SoundClip::new(samples, 48_000) {
        Ok(clip) => clip,
        Err(error) => panic!("tone clip: {error}"),
    };
    SoundAsset::from_clip(&clip)
}

fn engine_patch() -> AudioPatch {
    let def = AudioPatchDef {
        version: 1,
        name: "engine".into(),
        bus: Some("Sfx".into()),
        parameters: vec![PatchParamDef {
            name: "speed".into(),
            min: 0.0,
            max: 100.0,
            smoothing_secs: 0.08,
            binding: None,
        }],
        layers: vec![PatchLayerDef {
            name: "engine_set".into(),
            sound: None,
            keys: vec![
                PatchKeyDef {
                    value: 0.0,
                    sound: "idle".into(),
                    gain_db: 0.0,
                    pitch: 1.0,
                },
                PatchKeyDef {
                    value: 50.0,
                    sound: "mid".into(),
                    gain_db: 0.0,
                    pitch: 1.0,
                },
                PatchKeyDef {
                    value: 100.0,
                    sound: "fast".into(),
                    gain_db: 0.0,
                    pitch: 1.0,
                },
            ],
            parameter: Some("speed".into()),
            volume_db: -6.0,
            volume_curves: vec![],
            pitch_curves: vec![PatchCurveDef {
                parameter: "speed".into(),
                points: vec![(0.0, 0.6), (100.0, 1.4)],
            }],
            filter_curves: vec![],
            one_shot: None,
            granular: None,
        }],
    };
    let mut key_sounds = HashMap::new();
    key_sounds.insert(("engine_set".to_string(), 0), looping_tone(90.0));
    key_sounds.insert(("engine_set".to_string(), 1), looping_tone(180.0));
    key_sounds.insert(("engine_set".to_string(), 2), looping_tone(360.0));
    AudioPatch {
        def,
        layer_sounds: HashMap::new(),
        key_sounds,
    }
}

fn crossings(frames: &[f32]) -> usize {
    let mono: Vec<f32> = frames
        .chunks(2)
        .map(|c| (c[0] + c.get(1).copied().unwrap_or(c[0])) * 0.5)
        .collect();
    mono.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count()
}

#[test]
fn a_settled_patch_ticks_without_allocating_and_still_changes_pitch() {
    let mut audio = headless();
    let id = match audio.spawn_patch(engine_patch()) {
        Ok(id) => id,
        Err(error) => panic!("spawn patch: {error}"),
    };
    audio.tick(1.0 / 60.0);
    let before = allocations();
    for _ in 0..120 {
        audio.tick(1.0 / 60.0);
    }
    let allocs = allocations() - before;
    assert_eq!(allocs, 0, "120 settled patch ticks allocated {allocs} times");

    let idle = match audio.render_offline(4_096) {
        Ok(frames) => frames,
        Err(error) => panic!("render idle: {error}"),
    };
    match audio.patch_set_param(id, "speed", 100.0) {
        Ok(()) => {}
        Err(error) => panic!("set speed: {error}"),
    }
    for _ in 0..120 {
        audio.tick(1.0 / 60.0);
    }
    let fast = match audio.render_offline(4_096) {
        Ok(frames) => frames,
        Err(error) => panic!("render fast: {error}"),
    };
    let (early, late) = (crossings(&idle), crossings(&fast));
    assert!(
        early > 0 && late > early * 3 / 2,
        "the mix still moves when speed changes (zero crossings {early} idle, {late} fast)"
    );
}
