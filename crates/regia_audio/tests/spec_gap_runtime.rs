// SPDX-License-Identifier: MPL-2.0
//! Spec 16 runtime-gap conformance (lane sprint-spec-gap-audio-runtime-k, ADR-5699): the runtime
//! audio features the spec-16 audit (ADR-5614) marked open work, exercised through the public API.
//!
//! - § 2 sound-asset playback defaults (loop region / volume / pitch / bus),
//! - § 3 a cue in 3D (`Audio::play_cue_at` / `Audio::play_cue_on`),
//! - § 3 an `AudioEmitter` playing a `SoundCue` (the new `cue`/`cue_name` + `CueAssets`).
//!
//! Headless offline mixer, fixed frame steps; no wall clock. (§ 1 `config/audio.ron` at runtime is
//! `regia::DefaultPlugins` and is tested in `crates/regia/src/lib.rs::audio_config_tests`.)

use std::time::Duration;

use regia_audio::{
    Audio, AudioEmitter, AudioOcclusion, AudioSettings, CueAssets, CueEntry, EmitterFrame, EmitterUpdate,
    LoopRegion, PlaybackParams, SoundAsset, SoundAssets, SoundClip, SoundCue,
};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;

const RATE: u32 = 48_000;
const FRAME: f64 = 1.0 / 60.0;

fn headless() -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    })
}

/// A sine asset of `seconds` at `hz`.
fn tone(hz: f32, seconds: f64) -> SoundAsset {
    let len = (seconds * f64::from(RATE)) as usize;
    let samples: Vec<f32> = (0..len)
        .map(|i| ((i as f32 / RATE as f32) * hz * std::f32::consts::TAU).sin() * 0.5)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("sine clip"))
}

/// `count` frames of tick + render; returns the interleaved stereo output.
fn run(audio: &mut Audio, count: usize) -> Vec<f32> {
    let mut out = Vec::new();
    for _ in 0..count {
        audio.tick(FRAME);
        out.extend_from_slice(
            &audio
                .render_offline((f64::from(RATE) * FRAME) as usize)
                .expect("offline render"),
        );
    }
    out
}

fn rms(frames: &[f32]) -> f32 {
    (frames.iter().map(|s| s * s).sum::<f32>() / frames.len().max(1) as f32).sqrt()
}

/// Spec 16 § 2: the sound asset's default volume adds to a play's and its default loop region
/// makes a one-shot-length clip loop.
#[test]
fn spec16_section2_sound_asset_default_volume_and_loop_region_apply_to_a_play() {
    // Volume: −6 dB ≈ half amplitude.
    let quiet = tone(440.0, 0.2).with_default_volume_db(-6.0);
    let full = tone(440.0, 0.2);
    let mut audio = headless();
    audio.start_asset(&full, Default::default()).expect("plays");
    let loud = rms(&run(&mut audio, 6));
    let mut audio = headless();
    audio.start_asset(&quiet, Default::default()).expect("plays");
    let soft = rms(&run(&mut audio, 6));
    assert!(
        (soft / loud - 0.5).abs() < 0.05,
        "−6 dB default ≈ half amplitude: {soft} vs {loud}"
    );

    // Loop region: a 0.05 s clip with a default loop keeps sounding past 0.05 s of render.
    let clip = tone(440.0, 0.05).with_default_loop_region(LoopRegion {
        start_secs: 0.0,
        end_secs: None,
    });
    let mut audio = headless();
    audio.start_asset(&clip, Default::default()).expect("plays");
    let out = run(&mut audio, 8); // ~0.13 s of rendered audio
    let tail: Vec<f32> = out.iter().skip(out.len() / 2).copied().collect();
    assert!(
        rms(&tail) > 1e-3,
        "the default loop region keeps the tail audible"
    );
}

/// Spec 16 § 2: the asset's default pitch multiplies the play's.
#[test]
fn spec16_section2_sound_asset_default_pitch_multiplies_the_plays() {
    // Pitch 2 doubles the frequency: a 0.25 s window holds about twice the zero crossings.
    let count = |asset: &SoundAsset, pitch: f32| {
        let mut audio = headless();
        audio
            .start_asset(
                asset,
                PlaybackParams {
                    pitch,
                    ..Default::default()
                },
            )
            .expect("plays");
        let out = run(&mut audio, 15); // 0.25 s
        let mono: Vec<f32> = out.iter().step_by(2).copied().collect();
        mono.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count()
    };
    let asset = tone(200.0, 1.0).with_default_pitch(2.0);
    let up = count(&asset, 1.0);
    let flat = count(&tone(200.0, 1.0), 1.0);
    assert!(
        up > flat * 3 / 2,
        "default pitch 2 raises the rate: {up} vs {flat} crossings"
    );
}

/// Spec 16 § 3: `play_cue_at` plays a cue variation at a position (spatialised, audible).
#[test]
fn spec16_section3_play_cue_at_plays_a_variation_at_a_position() {
    let mut audio = headless();
    let mut cue = SoundCue::new(
        "hit",
        vec![CueEntry {
            asset: tone(330.0, 0.2),
            weight: 1.0,
        }],
    );
    cue.max_concurrent = None;
    audio.register_cue(cue);
    let emitter = AudioEmitter::default();
    let id = audio
        .play_cue_at("hit", Vec3::new(2.0, 0.0, 0.0), &emitter)
        .expect("plays")
        .expect("audible near the listener");
    let _ = id;
    assert!(rms(&run(&mut audio, 6)) > 1e-3, "the positional cue is audible");
}

/// Spec 16 § 3: `play_cue_on` attaches a cue variation to an entity's emitter.
#[test]
fn spec16_section3_play_cue_on_attaches_the_variation_to_the_entity() {
    let mut audio = headless();
    let mut cue = SoundCue::new(
        "hit",
        vec![CueEntry {
            asset: tone(330.0, 0.2),
            weight: 1.0,
        }],
    );
    cue.max_concurrent = None;
    audio.register_cue(cue);
    let entity = Entity::from_raw_u32(11).expect("entity");
    let emitter = AudioEmitter::default();
    let id = audio
        .play_cue_on("hit", entity, &emitter)
        .expect("plays")
        .expect("started");
    let _ = id;
    let status = audio
        .emitter_status()
        .into_iter()
        .find(|s| s.entity == entity)
        .expect("the entity's emitter is tracked");
    assert_eq!(status.voices, 1, "the cue variation follows the entity");
    assert!(rms(&run(&mut audio, 6)) > 1e-3);
}

/// Spec 16 § 3: an `AudioEmitter` with a `cue` autoplays one of the cue's variations, and a `sound`
/// takes precedence: an emitter naming both plays the sound (rev-kimi-10 finding 10). The cue's
/// only entry is silent and the sound is a loud tone, so the output tells which one plays.
#[test]
fn spec16_section3_an_emitter_cue_autoplays_and_sound_wins() {
    let silent = SoundAsset::from_clip(&SoundClip::new(vec![0.0; 9_600], RATE).expect("silent clip"));
    let mut cues = CueAssets::default();
    let mut cue = SoundCue::new(
        "steps",
        vec![CueEntry {
            asset: silent,
            weight: 1.0,
        }],
    );
    cue.max_concurrent = None;
    cues.add("steps", cue);
    let mut sounds = SoundAssets::default();
    sounds.add("hum", tone(220.0, 1.0));

    let entity = Entity::from_raw_u32(7).expect("entity");
    let level_of = |emitter: AudioEmitter| {
        let mut audio = headless();
        let update = EmitterUpdate {
            entity,
            emitter,
            frame: EmitterFrame {
                position: Vec3::new(0.0, 0.0, -1.0),
                ..Default::default()
            },
        };
        let mut out = Vec::new();
        for _ in 0..6 {
            audio.sync_emitter_frames(
                std::slice::from_ref(&update),
                FRAME as f32,
                Some(&sounds),
                Some(&cues),
                None,
            );
            out.extend(run(&mut audio, 1));
        }
        let voices = audio
            .emitter_status()
            .into_iter()
            .find(|s| s.entity == entity)
            .map_or(0, |s| s.voices);
        (voices, rms(&out))
    };
    // The cue alone autoplays its (silent) variation.
    let (voices, level) = level_of(AudioEmitter {
        cue_name: "steps".into(),
        ..AudioEmitter::default()
    });
    assert_eq!(voices, 1, "the emitter autoplayed a cue variation");
    assert!(level < 1e-4, "the cue's entry is silent: {level}");
    // Sound and cue both named: the sound plays, the cue does not.
    let (voices, level) = level_of(AudioEmitter {
        sound_name: "hum".into(),
        cue_name: "steps".into(),
        ..AudioEmitter::default()
    });
    assert_eq!(voices, 1, "one voice: the sound's");
    assert!(level > 1e-2, "the sound won over the cue: {level}");

    // A cooldown on a manually played cue still applies in 3D.
    let mut audio = headless();
    let mut cue = SoundCue::new(
        "thump",
        vec![CueEntry {
            asset: tone(110.0, 0.1),
            weight: 1.0,
        }],
    );
    cue.max_concurrent = None;
    cue.cooldown = Duration::from_millis(100);
    audio.register_cue(cue);
    let emitter = AudioEmitter::default();
    assert!(
        audio
            .play_cue_at("thump", Vec3::ZERO, &emitter)
            .expect("plays")
            .is_some()
    );
    assert!(
        audio
            .play_cue_at("thump", Vec3::ZERO, &emitter)
            .expect("plays")
            .is_none(),
        "the cooldown applies to a positional cue play"
    );
}

/// Spec 16 § 3 "attached: an AudioEmitter entity with `looping: false`" is a one-shot: a cue
/// emitter synced every frame plays its variation once and stays silent, like a sound emitter
/// (rev-kimi-10 finding 1, the review's probe (a)).
#[test]
fn spec16_section3_a_non_looping_cue_emitter_is_a_one_shot() {
    let entity = Entity::from_raw_u32(7).expect("entity");
    let frame = EmitterFrame {
        position: Vec3::new(0.0, 0.0, -2.0),
        ..Default::default()
    };
    let voices_of = |audio: &Audio| {
        audio
            .emitter_status()
            .into_iter()
            .find(|s| s.entity == entity)
            .map_or(0, |s| s.voices)
    };
    // Control: a non-looping sound emitter with the same 0.05 s clip.
    let mut audio = headless();
    let mut sounds = SoundAssets::default();
    sounds.add("blip", tone(220.0, 0.05));
    let update = EmitterUpdate {
        entity,
        emitter: AudioEmitter {
            sound_name: "blip".into(),
            looping: false,
            ..AudioEmitter::default()
        },
        frame,
    };
    let mut sound_early = 0;
    let mut sound_late = 0;
    for i in 0..60 {
        audio.sync_emitter_frames(
            std::slice::from_ref(&update),
            FRAME as f32,
            Some(&sounds),
            None,
            None,
        );
        run(&mut audio, 1);
        if i < 2 {
            sound_early += voices_of(&audio);
        } else if i >= 30 {
            sound_late += voices_of(&audio);
        }
    }
    assert!(sound_early > 0, "control: the sound emitter played");
    assert_eq!(sound_late, 0, "control: the non-looping sound emitter is done");

    let mut audio = headless();
    let mut cues = CueAssets::default();
    let mut cue = SoundCue::new(
        "blip",
        vec![CueEntry {
            asset: tone(220.0, 0.05),
            weight: 1.0,
        }],
    );
    cue.max_concurrent = None;
    cues.add("blip", cue);
    let update = EmitterUpdate {
        entity,
        emitter: AudioEmitter {
            cue_name: "blip".into(),
            looping: false,
            ..AudioEmitter::default()
        },
        frame,
    };
    let mut cue_early = 0;
    let mut cue_late = 0;
    let mut cue_late_rms = 0.0f32;
    for i in 0..60 {
        audio.sync_emitter_frames(
            std::slice::from_ref(&update),
            FRAME as f32,
            None,
            Some(&cues),
            None,
        );
        let out = run(&mut audio, 1);
        if i < 2 {
            cue_early += voices_of(&audio);
        } else if i >= 30 {
            cue_late += voices_of(&audio);
            cue_late_rms = cue_late_rms.max(rms(&out));
        }
    }
    assert!(cue_early > 0, "the cue emitter played its variation");
    assert_eq!(
        cue_late, 0,
        "the non-looping cue emitter retriggered: {cue_late} voice-frames after 0.5 s, peak rms {cue_late_rms}"
    );
}

/// A looping cue emitter loops the variation it picked (the voice does not end), and only a cue
/// change starts a new pick.
#[test]
fn spec16_section3_a_looping_cue_emitter_loops_its_variation() {
    let entity = Entity::from_raw_u32(8).expect("entity");
    let mut audio = headless();
    let mut cues = CueAssets::default();
    let mut cue = SoundCue::new(
        "hum",
        vec![CueEntry {
            asset: tone(220.0, 0.05),
            weight: 1.0,
        }],
    );
    cue.max_concurrent = None;
    cues.add("hum", cue);
    let update = EmitterUpdate {
        entity,
        emitter: AudioEmitter {
            cue_name: "hum".into(),
            looping: true,
            ..AudioEmitter::default()
        },
        frame: EmitterFrame {
            position: Vec3::new(0.0, 0.0, -2.0),
            ..Default::default()
        },
    };
    let mut late_rms = f32::MAX;
    for i in 0..60 {
        audio.sync_emitter_frames(
            std::slice::from_ref(&update),
            FRAME as f32,
            None,
            Some(&cues),
            None,
        );
        let out = run(&mut audio, 1);
        if i >= 30 {
            late_rms = late_rms.min(rms(&out));
        }
    }
    let voices = audio
        .emitter_status()
        .into_iter()
        .find(|s| s.entity == entity)
        .map_or(0, |s| s.voices);
    assert_eq!(voices, 1, "one looping voice, never re-started");
    assert!(
        late_rms > 1e-2,
        "the 0.05 s variation keeps looping past 0.5 s: {late_rms}"
    );
}

/// Spec 16 § 2 on a positional play: the asset's default volume survives the spatial gain
/// (rev-kimi-10 finding 2, the review's probe (b)): a −40 dB default is about 40 dB quieter.
#[test]
fn spec16_section2_the_asset_default_volume_applies_to_a_positional_play() {
    let emitter = AudioEmitter::default();
    let mut level = Vec::new();
    for default_db in [0.0f32, -40.0] {
        let mut audio = headless();
        let mut cue = SoundCue::new(
            "hit",
            vec![CueEntry {
                asset: tone(330.0, 0.2).with_default_volume_db(default_db),
                weight: 1.0,
            }],
        );
        cue.max_concurrent = None;
        audio.register_cue(cue);
        audio
            .play_cue_at("hit", Vec3::new(2.0, 0.0, 0.0), &emitter)
            .expect("plays")
            .expect("audible");
        level.push(rms(&run(&mut audio, 6)));
    }
    assert!(level[0] > 1e-3, "control audible: {level:?}");
    let db = 20.0 * (level[1] / level[0]).log10();
    assert!(
        (db + 40.0).abs() < 1.0,
        "the −40 dB asset default is about 40 dB down on a positional play: {db} dB ({level:?})"
    );
}

/// Spec 16 § 2 on an emitter: the asset's default volume and pitch hold through every per-frame
/// spatial update (rev-kimi-10 finding 2).
#[test]
fn spec16_section2_the_asset_defaults_apply_to_an_emitter_voice() {
    let entity = Entity::from_raw_u32(9).expect("entity");
    let measure = |asset: SoundAsset| {
        let mut audio = headless();
        let mut sounds = SoundAssets::default();
        sounds.add("hum", asset);
        let update = EmitterUpdate {
            entity,
            emitter: AudioEmitter {
                sound_name: "hum".into(),
                ..AudioEmitter::default()
            },
            frame: EmitterFrame {
                position: Vec3::new(0.0, 0.0, -2.0),
                ..Default::default()
            },
        };
        let mut out = Vec::new();
        for _ in 0..30 {
            audio.sync_emitter_frames(
                std::slice::from_ref(&update),
                FRAME as f32,
                Some(&sounds),
                None,
                None,
            );
            out.extend(run(&mut audio, 1));
        }
        // The last 0.25 s, after every tween has settled.
        let tail = &out[out.len() / 2..];
        let mono: Vec<f32> = tail.iter().step_by(2).copied().collect();
        let crossings = mono.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        (rms(tail), crossings)
    };
    let (plain, plain_crossings) = measure(tone(200.0, 1.0));
    let (quiet, _) = measure(tone(200.0, 1.0).with_default_volume_db(-20.0));
    let (_, up_crossings) = measure(tone(200.0, 1.0).with_default_pitch(2.0));
    let db = 20.0 * (quiet / plain).log10();
    assert!(
        (db + 20.0).abs() < 1.0,
        "−20 dB default on an emitter voice: {db} dB"
    );
    assert!(
        up_crossings > plain_crossings * 3 / 2,
        "default pitch 2 holds on an emitter voice: {up_crossings} vs {plain_crossings} crossings"
    );
}

/// Spec 16 § 3: a cue's authored low-pass applies to a positional play (rev-kimi-10 finding 3):
/// a 6 kHz tone through a 300 Hz cue filter is far quieter than through an unfiltered cue.
#[test]
fn spec16_section3_play_cue_at_applies_the_cues_low_pass() {
    let emitter = AudioEmitter::default();
    let level = |cutoff: Option<f32>| {
        let mut audio = headless();
        let mut cue = SoundCue::new(
            "zap",
            vec![CueEntry {
                asset: tone(6_000.0, 0.2),
                weight: 1.0,
            }],
        );
        cue.max_concurrent = None;
        cue.filter_cutoff_hz = cutoff;
        audio.register_cue(cue);
        audio
            .play_cue_at("zap", Vec3::new(2.0, 0.0, 0.0), &emitter)
            .expect("plays")
            .expect("audible");
        rms(&run(&mut audio, 6))
    };
    let open = level(None);
    let muffled = level(Some(300.0));
    assert!(open > 1e-3, "control audible: {open}");
    assert!(
        muffled < open * 0.2,
        "the cue's 300 Hz low-pass muffles the positional 6 kHz play: {muffled} vs {open}"
    );
}

/// An emitter's `volume_db` applies to a cue emitter as it does to a sound emitter (rev-kimi-10
/// finding 6).
#[test]
fn spec16_section3_a_cue_emitter_follows_the_emitters_volume() {
    let entity = Entity::from_raw_u32(10).expect("entity");
    let mut cues = CueAssets::default();
    let mut cue = SoundCue::new(
        "hum",
        vec![CueEntry {
            asset: tone(220.0, 1.0),
            weight: 1.0,
        }],
    );
    cue.max_concurrent = None;
    cues.add("hum", cue);
    let level = |volume_db: f32| {
        let mut audio = headless();
        let update = EmitterUpdate {
            entity,
            emitter: AudioEmitter {
                cue_name: "hum".into(),
                volume_db,
                ..AudioEmitter::default()
            },
            frame: EmitterFrame {
                position: Vec3::new(0.0, 0.0, -2.0),
                ..Default::default()
            },
        };
        let mut out = Vec::new();
        for _ in 0..20 {
            audio.sync_emitter_frames(
                std::slice::from_ref(&update),
                FRAME as f32,
                None,
                Some(&cues),
                None,
            );
            out.extend(run(&mut audio, 1));
        }
        rms(&out[out.len() / 2..])
    };
    let full = level(0.0);
    let quiet = level(-20.0);
    let db = 20.0 * (quiet / full).log10();
    assert!(
        (db + 20.0).abs() < 1.0,
        "emitter −20 dB on a cue emitter: {db} dB ({quiet} vs {full})"
    );
}

/// One emitter update at `position` for `emitter`.
fn update_at(entity: Entity, emitter: AudioEmitter, position: Vec3) -> EmitterUpdate {
    EmitterUpdate {
        entity,
        emitter,
        frame: EmitterFrame {
            position,
            ..Default::default()
        },
    }
}

/// A one-entry cue of a `hz` tone lasting `seconds`, with the authored low-pass `cutoff`.
fn tone_cue(name: &str, hz: f32, seconds: f64, cutoff: Option<f32>) -> SoundCue {
    let mut cue = SoundCue::new(
        name,
        vec![CueEntry {
            asset: tone(hz, seconds),
            weight: 1.0,
        }],
    );
    cue.max_concurrent = None;
    cue.filter_cutoff_hz = cutoff;
    cue
}

/// Spec 16 § 3: a cue's authored low-pass applies to an attached play too (fixrev6, review
/// sprint-rev-claude-22-k finding 6; ADR-5977): `play_cue_on` with a 300 Hz cue filter muffles a
/// 6 kHz variation like `play_cue_at` does.
#[test]
fn spec16_section3_play_cue_on_applies_the_cues_low_pass() {
    let entity = Entity::from_raw_u32(12).expect("entity");
    let emitter = AudioEmitter::default();
    let level = |cutoff: Option<f32>| {
        let mut audio = headless();
        audio.register_cue(tone_cue("zap", 6_000.0, 0.2, cutoff));
        audio
            .play_cue_on("zap", entity, &emitter)
            .expect("plays")
            .expect("started");
        rms(&run(&mut audio, 6))
    };
    let open = level(None);
    let muffled = level(Some(300.0));
    assert!(open > 1e-3, "control audible: {open}");
    assert!(
        muffled < open * 0.2,
        "the cue's 300 Hz low-pass muffles the attached 6 kHz play: {muffled} vs {open}"
    );
}

/// Spec 16 § 3: an `AudioEmitter` that autoplays a cue applies the cue's authored low-pass too
/// (ADR-5977): the same cue sounds the same whether `play_cue_on` or an emitter plays it.
#[test]
fn spec16_section3_a_cue_emitter_applies_the_cues_low_pass() {
    let entity = Entity::from_raw_u32(13).expect("entity");
    let level = |cutoff: Option<f32>| {
        let mut audio = headless();
        let mut cues = CueAssets::default();
        cues.add("buzz", tone_cue("buzz", 6_000.0, 1.0, cutoff));
        let update = update_at(
            entity,
            AudioEmitter {
                cue_name: "buzz".into(),
                ..AudioEmitter::default()
            },
            Vec3::new(0.0, 0.0, -2.0),
        );
        let mut out = Vec::new();
        for _ in 0..12 {
            audio.sync_emitter_frames(
                std::slice::from_ref(&update),
                FRAME as f32,
                None,
                Some(&cues),
                None,
            );
            out.extend(run(&mut audio, 1));
        }
        rms(&out[out.len() / 2..])
    };
    let open = level(None);
    let muffled = level(Some(300.0));
    assert!(open > 1e-3, "control audible: {open}");
    assert!(
        muffled < open * 0.2,
        "the cue's 300 Hz low-pass muffles the emitter's 6 kHz variation: {muffled} vs {open}"
    );
}

/// ADR-5977: an attached cue voice keeps its low-pass when its emitter's track is rebuilt (here
/// the emitter moves to another bus, so `play_cue_on`'s `World` track is replaced by an `Sfx`
/// one and the live voice moves over) — the move rebuilds the filter under the new track instead
/// of dropping it.
#[test]
fn spec16_section3_play_cue_on_keeps_the_low_pass_across_a_track_move() {
    let entity = Entity::from_raw_u32(14).expect("entity");
    let level = |cutoff: Option<f32>| {
        let mut audio = headless();
        audio.register_cue(tone_cue("whine", 6_000.0, 1.0, cutoff));
        audio
            .play_cue_on("whine", entity, &AudioEmitter::default())
            .expect("plays")
            .expect("started");
        let _ = run(&mut audio, 2);
        // The emitter now names another bus: the first sync rebuilds its track there.
        let update = update_at(
            entity,
            AudioEmitter {
                bus: Some("Sfx".into()),
                ..AudioEmitter::default()
            },
            Vec3::ZERO,
        );
        let mut out = Vec::new();
        for _ in 0..12 {
            audio.sync_emitter_frames(std::slice::from_ref(&update), FRAME as f32, None, None, None);
            out.extend(run(&mut audio, 1));
        }
        let status = audio
            .emitter_status()
            .into_iter()
            .find(|s| s.entity == entity)
            .expect("the entity's emitter is tracked");
        assert_eq!(status.voices, 1, "the cue voice moved with the track");
        rms(&out[out.len() / 2..])
    };
    let open = level(None);
    let muffled = level(Some(300.0));
    assert!(open > 1e-3, "control audible after the move: {open}");
    assert!(
        muffled < open * 0.2,
        "the cue's low-pass survives the track move: {muffled} vs {open}"
    );
}

/// ADR-5977: the cue's low-pass track sits under the emitter's own track, so the emitter's
/// occlusion filter still applies to a filtered attached cue voice (both filters stack). A 1 kHz
/// variation through an open 8 kHz cue filter drops when the emitter is fully occluded.
#[test]
fn spec16_section3_a_filtered_attached_cue_still_follows_occlusion() {
    let entity = Entity::from_raw_u32(15).expect("entity");
    let level = |occluded: bool| {
        let mut audio = headless();
        audio.register_cue(tone_cue("drone", 1_000.0, 1.0, Some(8_000.0)));
        audio
            .play_cue_on("drone", entity, &AudioEmitter::default())
            .expect("plays")
            .expect("started");
        let mut occlusion = AudioOcclusion::default();
        if occluded {
            occlusion.report(entity, 1.0);
        }
        let update = update_at(entity, AudioEmitter::default(), Vec3::new(0.0, 0.0, -2.0));
        let mut out = Vec::new();
        for _ in 0..30 {
            audio.sync_emitter_frames(
                std::slice::from_ref(&update),
                FRAME as f32,
                None,
                None,
                Some(&mut occlusion),
            );
            if occluded {
                occlusion.report(entity, 1.0);
            }
            out.extend(run(&mut audio, 1));
        }
        rms(&out[out.len() * 3 / 4..])
    };
    let open = level(false);
    let occluded = level(true);
    assert!(open > 1e-3, "control audible: {open}");
    assert!(
        occluded < open * 0.7,
        "occlusion still muffles a filtered attached cue voice: {occluded} vs {open}"
    );
}
