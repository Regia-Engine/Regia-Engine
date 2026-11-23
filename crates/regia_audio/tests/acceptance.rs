// SPDX-License-Identifier: MPL-2.0
//! M8 acceptance tests (`docs/specs/16-audio.md` § 6), run end to end through the offline
//! renderer: no audio device is needed and every assertion is on rendered samples or observable
//! mixer state.
//!
//! Streaming tests pace rendering at real time (one 1/60 s frame per ~1/60 s of wall time):
//! kira's streaming decoder fills its ring buffer from a background thread, which cannot keep up
//! when the offline renderer consumes faster than real time — a faster-than-real-time consumer
//! would measure decoder starvation, not engine behaviour.

use std::sync::Arc;
use std::time::{Duration, Instant};

use regia_audio::{
    AnnouncePriority, AnnouncerBank, Audio, AudioEmitter, AudioPatch, AudioPatchDef, AudioSettings,
    AudioSnapshot, CueEntry, CuePlayMode, ListenerFrame, MusicAsset, MusicStem, PatchCurveDef, PatchKeyDef,
    PatchLayerDef, PatchParamDef, PlaybackParams, RolloffCurve, SoundAsset, SoundClip, SoundCue, StealPolicy,
};
use regia_core::math::Vec3;

const RATE: u32 = 48_000;
const FRAME: f64 = 1.0 / 60.0;

fn headless() -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    })
}

fn headless_with(limit: usize) -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        voice_limit: Some(limit),
        ..AudioSettings::default()
    })
}

/// A sine-wave clip asset.
fn tone(hz: f32, seconds: f64, amp: f32) -> SoundAsset {
    let len = (seconds * f64::from(RATE)) as usize;
    let samples: Vec<f32> = (0..len)
        .map(|i| ((i as f32 / RATE as f32) * hz * std::f32::consts::TAU).sin() * amp)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("sine clip"))
}

/// A looping tone asset.
fn looping_tone(hz: f32, amp: f32) -> SoundAsset {
    tone(hz, 0.5, amp)
}

/// WAV (PCM16 mono) bytes of a sine, for streaming tests.
fn wav_bytes(hz: f32, seconds: f64, amp: f32) -> Arc<Vec<u8>> {
    let len = (seconds * f64::from(RATE)) as usize;
    let mut data = Vec::with_capacity(len * 2);
    for i in 0..len {
        let sample = (((i as f32 / RATE as f32) * hz * std::f32::consts::TAU).sin() * amp * 32_000.0) as i16;
        data.extend_from_slice(&sample.to_le_bytes());
    }
    let mut wav = Vec::with_capacity(44 + data.len());
    wav.extend_from_slice(b"RIFF");
    let chunk = (36 + data.len()) as u32;
    wav.extend_from_slice(&chunk.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    Arc::new(wav)
}

fn streaming_tone(hz: f32, seconds: f64, amp: f32, label: &str) -> SoundAsset {
    SoundAsset::streaming_from_bytes(wav_bytes(hz, seconds, amp), label).expect("wav parses")
}

/// Splits interleaved stereo into left/right.
fn channels(frames: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let left = frames.iter().step_by(2).copied().collect();
    let right = frames.iter().skip(1).step_by(2).copied();
    (left, right.collect())
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

/// Goertzel filter magnitude at `hz` over one channel — detects whether a tone is present.
/// For a sine of peak amplitude `a` this reads ≈ `a / 2`.
fn goertzel(samples: &[f32], hz: f32, rate: f32) -> f32 {
    let k = 2.0 * std::f32::consts::PI * hz / rate;
    let (coeff, s1, s2) = (2.0 * k.cos(), 0.0f32, 0.0f32);
    let mut s1 = s1;
    let mut s2 = s2;
    for &x in samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() / samples.len().max(1) as f32
}

/// The larger of the per-channel Goertzel magnitudes (mono content is identical on both
/// channels; panned content is caught by the stronger ear).
fn tone_level(frames: &[f32], hz: f32) -> f32 {
    let (left, right) = channels(frames);
    goertzel(&left, hz, RATE as f32).max(goertzel(&right, hz, RATE as f32))
}

/// Runs `seconds` of audio: tick + render per 1/60 s frame, paced at real time so streaming
/// decoders (background threads with a 0.34 s ring buffer) always keep up, concatenating output.
fn run(audio: &mut Audio, seconds: f64) -> Vec<f32> {
    let mut out = Vec::new();
    let frames_per_tick = (RATE as f64 * FRAME) as usize;
    let ticks = (seconds / FRAME).round() as usize;
    let step = Duration::from_secs_f64(FRAME);
    for _ in 0..ticks {
        let start = Instant::now();
        audio.tick(FRAME);
        out.extend_from_slice(&audio.render_offline(frames_per_tick).expect("offline"));
        let elapsed = start.elapsed();
        if elapsed < step {
            std::thread::sleep(step - elapsed);
        }
    }
    out
}

fn ticks(audio: &mut Audio, count: usize) {
    for _ in 0..count {
        audio.tick(FRAME);
        let _ = audio.render_offline((RATE as f64 * FRAME) as usize);
    }
}

// --------------------------------------------------------------------- spatial audio (§ 3, § 6)

/// Kira pans with constant-power gains (−3 dB centre): at pan `p` the two channel gains are
/// `sqrt((1∓p)/2)`, so a source 45° off-axis (pan ≈ −0.707) reads ≈ 2.4× stronger on the near
/// ear, and a source straight to the side (pan −1) is silent on the far ear.
#[test]
fn panning_is_audible_left_and_right() {
    let mut audio = headless();
    audio.set_listener(ListenerFrame::looking_at(Vec3::ZERO, -Vec3::Z));

    let emitter = AudioEmitter {
        min_distance: 2.0,
        max_distance: 20.0,
        rolloff: RolloffCurve::Linear,
        ..AudioEmitter::default()
    };

    // 45° to the left front: the left ear must clearly dominate, both ears audible.
    audio
        .play_asset_at(
            &looping_tone(440.0, 0.3),
            PlaybackParams::default(),
            Vec3::new(-5.0, 0.0, -5.0),
            &emitter,
        )
        .expect("play left");
    audio.drain();
    let left_run = run(&mut audio, 0.3);
    let (l, r) = channels(&left_run);
    let (l_rms, r_rms) = (rms(&l), rms(&r));
    assert!(
        l_rms > r_rms * 2.0,
        "left position must favour the left ear: {l_rms} vs {r_rms}"
    );
    assert!(l_rms > 0.05, "audible at all: {l_rms}");
    assert!(r_rms > 0.01, "constant-power keeps the far ear audible: {r_rms}");

    // Hard left (90° off-axis): the right ear is nearly silent.
    let mut audio = headless();
    audio.set_listener(ListenerFrame::looking_at(Vec3::ZERO, -Vec3::Z));
    audio
        .play_asset_at(
            &looping_tone(440.0, 0.3),
            PlaybackParams::default(),
            Vec3::new(-5.0, 0.0, 0.0),
            &emitter,
        )
        .expect("play hard left");
    audio.drain();
    let (l, r) = channels(&run(&mut audio, 0.3));
    assert!(
        rms(&l) > rms(&r) * 5.0,
        "90° off-axis must be essentially one-eared: {} vs {}",
        rms(&l),
        rms(&r)
    );

    // Mirrored on the right.
    let mut audio = headless();
    audio.set_listener(ListenerFrame::looking_at(Vec3::ZERO, -Vec3::Z));
    audio
        .play_asset_at(
            &looping_tone(440.0, 0.3),
            PlaybackParams::default(),
            Vec3::new(5.0, 0.0, -5.0),
            &emitter,
        )
        .expect("play right");
    audio.drain();
    let (l, r) = channels(&run(&mut audio, 0.3));
    assert!(
        rms(&r) > rms(&l) * 2.0,
        "right position must favour the right ear"
    );

    // Straight ahead: centred.
    let mut audio = headless();
    audio.set_listener(ListenerFrame::looking_at(Vec3::ZERO, -Vec3::Z));
    audio
        .play_asset_at(
            &looping_tone(440.0, 0.3),
            PlaybackParams::default(),
            Vec3::new(0.0, 0.0, -5.0),
            &emitter,
        )
        .expect("play center");
    audio.drain();
    let (l, r) = channels(&run(&mut audio, 0.3));
    assert!(
        (rms(&l) - rms(&r)).abs() < rms(&l).max(1e-4) * 0.2,
        "front-centered must be balanced: {} vs {}",
        rms(&l),
        rms(&r)
    );
}

#[test]
fn attenuation_is_audible_with_distance() {
    let mut audio = headless();
    audio.set_listener(ListenerFrame::looking_at(Vec3::ZERO, -Vec3::Z));
    let emitter = AudioEmitter {
        min_distance: 2.0,
        max_distance: 10.0,
        rolloff: RolloffCurve::Linear,
        ..AudioEmitter::default()
    };

    audio
        .play_asset_at(
            &looping_tone(440.0, 0.3),
            PlaybackParams::default(),
            Vec3::new(0.0, 0.0, -2.0),
            &emitter,
        )
        .expect("near play");
    audio.drain();
    let near = rms(&run(&mut audio, 0.3));

    let mut audio = headless();
    audio.set_listener(ListenerFrame::looking_at(Vec3::ZERO, -Vec3::Z));
    audio
        .play_asset_at(
            &looping_tone(440.0, 0.3),
            PlaybackParams::default(),
            Vec3::new(0.0, 0.0, -8.0),
            &emitter,
        )
        .expect("far play");
    audio.drain();
    let far = rms(&run(&mut audio, 0.3));

    // Linear rolloff min 2 / max 10: gain at 8 m is 0.25 → −12 dB → near/far ≈ 4.
    let ratio = near / far;
    assert!(
        (3.0..=5.5).contains(&ratio),
        "expected ≈4× RMS ratio from linear rolloff, got {ratio}"
    );
}

// --------------------------------------------------------------------- one-shots + virtualisation

/// 200 one-shots within one frame: the voice budget (64) is enforced at start time, the rest are
/// virtualised (kept as time cursors), and the rendered mix stays clean. The one-shots use
/// distinct frequencies so their sum is incoherent like real gameplay audio.
#[test]
fn two_hundred_one_shots_virtualise_to_the_voice_limit_without_glitches() {
    let mut audio = headless_with(64);
    let assets: Vec<SoundAsset> = (0..200)
        .map(|i| tone(400.0 + i as f32 * 3.0, 0.25, 0.05))
        .collect();
    for asset in &assets {
        audio
            .play_asset(asset, PlaybackParams::on_bus("Sfx"))
            .expect("enqueue");
    }
    audio.drain();
    assert_eq!(audio.played_count(), 200, "all plays accepted");
    assert_eq!(audio.real_voice_count(), 64, "voice budget enforced");
    assert_eq!(audio.virtual_voice_count(), 136, "the rest are virtualized");

    // Render under load: no NaN, no full-scale jumps (glitches), bounded output.
    let out = run(&mut audio, 0.12);
    assert!(out.len() >= 11_000, "rendered a full window: {}", out.len());
    let mut max_jump = 0.0f32;
    for pair in out
        .chunks(2)
        .flat_map(|c| [c[0], c[1]])
        .collect::<Vec<_>>()
        .windows(2)
    {
        max_jump = max_jump.max((pair[1] - pair[0]).abs());
    }
    for &s in &out {
        assert!(s.is_finite(), "no NaN/inf in the mix");
    }
    // 64 incoherent tones at amplitude 0.05: the sample-to-sample slope of the sum stays far
    // below click territory (a single full-scale click is ≥ 0.5).
    assert!(max_jump < 0.4, "no discontinuity glitches, max jump {max_jump}");

    // Voices retire as the short clips finish and the mix keeps going.
    ticks(&mut audio, 30); // 0.5 s
    assert!(
        audio.real_voice_count() + audio.virtual_voice_count() < 200,
        "finished voices retire"
    );
}

// --------------------------------------------------------------------- music (§ 3a)

fn mono_track(name: &str, hz: f32) -> MusicAsset {
    MusicAsset::from_stems(
        name,
        120.0,
        4,
        vec![MusicStem {
            name: "base".into(),
            asset: streaming_tone(hz, 1.2, 0.4, name),
            base_db: 0.0,
            intensity: vec![(0.0, 0.0), (1.0, 0.0)],
        }],
    )
}

#[test]
fn music_crossfades_between_two_streaming_tracks() {
    let mut audio = headless();
    let a = mono_track("a", 220.0);
    let b = mono_track("b", 330.0);

    audio.play_music(&a, Duration::ZERO).expect("track a");
    let first = run(&mut audio, 0.3);
    assert!(tone_level(&first, 220.0) > 0.05, "track A audible");

    let crossfade = Duration::from_secs_f32(0.4);
    audio.play_music(&b, crossfade).expect("track b");
    assert!(audio.music_crossfading(), "crossfade tail runs");
    let mid = run(&mut audio, 0.4);
    // Equal-power crossfade: total energy stays in a sane band...
    assert!(rms(&mid) > 0.05, "crossfade stays audible: {}", rms(&mid));
    // Constant power: the sum of two uncorrelated tones faded in/out holds the level of one
    // (a crossfade that never attenuated either stem would read about 1.41x).
    let ratio = rms(&mid) / rms(&first);
    assert!((0.7..1.2).contains(&ratio), "crossfade level ratio {ratio}");

    let after = run(&mut audio, 0.3);
    assert!(!audio.music_crossfading(), "crossfade finished");
    assert!(tone_level(&after, 330.0) > 0.05, "track B audible");
    assert!(
        tone_level(&after, 220.0) < 0.005,
        "track A faded out: {}",
        tone_level(&after, 220.0)
    );
    assert_eq!(audio.current_music(), Some("b"));
}

#[test]
fn music_stems_switch_intensity_on_bar_boundaries() {
    let mut audio = headless();
    // 240 BPM, 2 beats per bar → a bar every 0.5 s. The drums stem is silent at intensity 0
    // (−120 dB offset) and full at intensity 1.
    let stems = vec![
        MusicStem {
            name: "pad".into(),
            asset: streaming_tone(220.0, 3.0, 0.3, "pad"),
            base_db: 0.0,
            intensity: vec![(0.0, 0.0), (1.0, 0.0)],
        },
        MusicStem {
            name: "drums".into(),
            asset: streaming_tone(880.0, 3.0, 0.3, "drums"),
            base_db: 0.0,
            intensity: vec![(0.0, -120.0), (1.0, 0.0)],
        },
    ];
    let asset = MusicAsset::from_stems("combat", 240.0, 2, stems);
    audio.play_music(&asset, Duration::ZERO).expect("music");
    run(&mut audio, 0.3);

    // Intensity set mid-bar (0.3 s): the drums stem must NOT come up before the 0.5 s boundary.
    audio.set_music_intensity(1.0);
    let before_boundary = run(&mut audio, 0.2); // covers 0.3–0.5 s
    let (_, drums_level) = audio
        .music_stem_levels()
        .into_iter()
        .find(|(name, _)| name == "drums")
        .expect("drums stem");
    assert!(
        drums_level <= -59.0,
        "levels unchanged before the boundary: {drums_level}"
    );
    assert!(
        tone_level(&before_boundary, 880.0) < 1e-3,
        "drums stay silent before the bar boundary"
    );

    // After the boundary the stem fades in and becomes audible.
    let after_boundary = run(&mut audio, 0.4); // covers 0.5–0.9 s
    let (_, drums_level) = audio
        .music_stem_levels()
        .into_iter()
        .find(|(name, _)| name == "drums")
        .expect("drums stem");
    assert!(
        drums_level > -1.0,
        "levels applied at the boundary: {drums_level}"
    );
    assert!(
        tone_level(&after_boundary, 880.0) > 0.02,
        "drums audible after the bar boundary"
    );
}

// --------------------------------------------------------------------- snapshots + ducking

#[test]
fn paused_snapshot_blends_the_music_down_and_back() {
    let mut audio = headless();
    audio.register_snapshot(AudioSnapshot::new("Paused").with_bus_volume("Music", -18.0));
    audio
        .play_music(
            &MusicAsset::from_stems(
                "menu",
                120.0,
                4,
                vec![MusicStem {
                    name: "base".into(),
                    asset: streaming_tone(220.0, 2.0, 0.4, "menu"),
                    base_db: 0.0,
                    intensity: vec![(0.0, 0.0), (1.0, 0.0)],
                }],
            ),
            Duration::ZERO,
        )
        .expect("music");
    let loud = rms(&run(&mut audio, 0.3));

    audio
        .push_snapshot("Paused", Duration::from_secs_f32(0.25))
        .expect("snapshot");
    assert_eq!(audio.active_snapshot(), Some("Paused"));
    run(&mut audio, 0.4); // let the blend finish
    let quiet = rms(&run(&mut audio, 0.3));
    assert!(
        quiet < loud * 0.2,
        "music blends ≈18 dB down under Paused: {quiet} vs {loud}"
    );

    audio.pop_snapshot(Duration::from_secs_f32(0.25)).expect("pop");
    run(&mut audio, 0.4);
    let restored = rms(&run(&mut audio, 0.3));
    assert!(
        (restored / loud - 1.0).abs() < 0.3,
        "music blends back after pop: {restored} vs {loud}"
    );
}

/// The announcer ducks the game buses by −6 dB (measured on the music bed's tone, so the
/// announcer's own energy does not pollute the measurement) and releases 300 ms after the line.
#[test]
fn announcer_ducks_the_game_and_releases() {
    let mut audio = headless();
    // Music bed to measure ducking against (streaming, loops).
    audio
        .play_music(
            &MusicAsset::from_stems(
                "bed",
                120.0,
                4,
                vec![MusicStem {
                    name: "base".into(),
                    asset: streaming_tone(220.0, 4.0, 0.4, "bed"),
                    base_db: 0.0,
                    intensity: vec![(0.0, 0.0), (1.0, 0.0)],
                }],
            ),
            Duration::ZERO,
        )
        .expect("music");
    let unducked = run(&mut audio, 0.3);
    let bed = tone_level(&unducked, 220.0);
    assert!(bed > 0.05, "bed audible: {bed}");

    // An announcer line: a 0.6 s tone on the Voice bus.
    audio.register_announcer_bank(
        "first_blood",
        AnnouncerBank::new(vec![tone(660.0, 0.6, 0.5)], AnnouncePriority::High),
    );
    let outcome = audio.announce("first_blood");
    assert_eq!(outcome, regia_audio::AnnounceOutcome::Started);

    // Attack is 50 ms; after that the music bed must be ≈6 dB down.
    let _ = run(&mut audio, 0.1); // attack tail
    let ducked_run = run(&mut audio, 0.4);
    let ducked_bed = tone_level(&ducked_run, 220.0);
    assert!(
        ducked_bed < bed * 0.65,
        "music ducks ≈6 dB under the announcer: {ducked_bed} vs {bed}"
    );
    // The announcer line itself is audible.
    assert!(tone_level(&ducked_run, 660.0) > 0.02, "line audible");

    // The line ends 0.6 s after it started; release is 300 ms.
    let _ = run(&mut audio, 0.8);
    let released_run = run(&mut audio, 0.3);
    let released = tone_level(&released_run, 220.0);
    assert!(
        released > bed * 0.8,
        "duck releases after the announcer: {released} vs {bed}"
    );
}

// --------------------------------------------------------------------- cues (§ 2)

#[test]
fn cue_of_five_variations_never_repeats_the_last_two_end_to_end() {
    let mut audio = headless();
    // Distinct clip lengths identify the pick in `played()`.
    let entries: Vec<CueEntry> = (0..5)
        .map(|i| CueEntry {
            asset: tone(500.0 + i as f32 * 10.0, 0.05 + f64::from(i) * 0.001, 0.3),
            weight: 1.0,
        })
        .collect();
    let mut cue = SoundCue::new("hit", entries);
    cue.no_repeat_last = 2;
    cue.mode = CuePlayMode::Random;
    audio.register_cue(cue);

    let mut lengths: Vec<usize> = Vec::new();
    for _ in 0..120 {
        audio.play_cue("hit").expect("registered").expect("played");
        audio.drain();
        audio.tick(FRAME);
        let _ = audio.render_offline(8);
        let last = audio.played().last().map(|p| p.clip_len);
        if let Some(len) = last {
            lengths.push(len);
        }
    }
    assert!(lengths.len() >= 100, "picked {} times", lengths.len());
    for window in lengths.windows(3) {
        assert_ne!(window[2], window[1], "repeated the previous pick");
        assert_ne!(window[2], window[0], "repeated a pick two back");
    }
    let distinct = lengths.iter().copied().collect::<std::collections::HashSet<_>>();
    assert_eq!(distinct.len(), 5, "all five variations rotate");
}

#[test]
fn cue_steals_the_oldest_instance_at_the_limit() {
    let mut audio = headless();
    let mut cue = SoundCue::new(
        "shot",
        vec![
            CueEntry {
                asset: tone(400.0, 0.5, 0.3),
                weight: 1.0,
            },
            CueEntry {
                asset: tone(410.0, 0.5, 0.3),
                weight: 1.0,
            },
        ],
    );
    cue.max_concurrent = Some(1);
    cue.steal = StealPolicy::Oldest;
    audio.register_cue(cue);

    let first = audio.play_cue("shot").expect("registered").expect("first plays");
    ticks(&mut audio, 3);
    let second = audio
        .play_cue("shot")
        .expect("registered")
        .expect("second steals");
    assert_ne!(first, second);
    ticks(&mut audio, 3);
    // The first voice was stolen (stopping); only the second remains of this cue.
    assert_eq!(audio.real_voice_count(), 1, "one live voice of the cue");

    // StealPolicy::None refuses instead.
    let mut strict = SoundCue::new(
        "strict",
        vec![CueEntry {
            asset: tone(430.0, 0.5, 0.3),
            weight: 1.0,
        }],
    );
    strict.max_concurrent = Some(1);
    strict.steal = StealPolicy::None;
    audio.register_cue(strict);
    assert!(audio.play_cue("strict").expect("registered").is_some());
    ticks(&mut audio, 1);
    assert!(
        audio.play_cue("strict").expect("registered").is_none(),
        "policy None refuses the second play"
    );
}

// --------------------------------------------------------------------- parametric patches (§ 3a)

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
    // Keyed layer: one loop per key.
    let mut key_sounds = std::collections::HashMap::new();
    key_sounds.insert(("engine_set".to_string(), 0), looping_tone(90.0, 0.3));
    key_sounds.insert(("engine_set".to_string(), 1), looping_tone(180.0, 0.3));
    key_sounds.insert(("engine_set".to_string(), 2), looping_tone(360.0, 0.3));
    AudioPatch {
        def,
        layer_sounds: Default::default(),
        key_sounds,
    }
}

#[test]
fn engine_patch_speed_sweep_is_click_free() {
    let mut audio = headless();
    let patch = engine_patch();
    let id = audio.spawn_patch(patch).expect("patch spawns");

    let mut all_out: Vec<f32> = Vec::new();
    // 0 → 100 (1 s), hold (0.2 s), 100 → 0 (1 s).
    let mut plan: Vec<f32> = (0..60).map(|i| i as f32 / 60.0 * 100.0).collect();
    plan.extend(std::iter::repeat_n(100.0, 12));
    plan.extend((0..60).map(|i| 100.0 - i as f32 / 60.0 * 100.0));
    for &target in &plan {
        audio.patch_set_param(id, "speed", target).expect("param");
        audio.tick(FRAME);
        let chunk = audio
            .render_offline((RATE as f64 * FRAME) as usize)
            .expect("offline");
        all_out.extend_from_slice(&chunk);
    }

    // Audible throughout.
    assert!(rms(&all_out) > 0.02, "engine audible: {}", rms(&all_out));
    // No discontinuities: the largest sample-to-sample jump stays well under click territory.
    let mono: Vec<f32> = all_out.chunks(2).map(|c| (c[0] + c[1]) / 2.0).collect();
    let mut max_jump = 0.0f32;
    for w in mono.windows(2) {
        max_jump = max_jump.max((w[1] - w[0]).abs());
    }
    // The 360 Hz key at amplitude ~0.5 has a per-sample slope ≤ 2π·360·0.5/48000 ≈ 0.024.
    assert!(
        max_jump < 0.1,
        "sweep must be click-free (max sample jump {max_jump})"
    );

    // The sweep is actually audible as a change. Key crossfades are equal-power (the squared
    // weights sum to 1), so the level stays put by design (ADR-1342); what moves is the pitch
    // content: the fast end (360 Hz key, pitch up to 1.4) crosses zero far more often than the
    // idle end (90 / 180 Hz keys, pitch from 0.6).
    let quarter = mono.len() / 4;
    let crossings = |s: &[f32]| s.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
    let (early, late) = (
        crossings(&mono[..quarter]),
        crossings(&mono[quarter * 2..quarter * 3]),
    );
    assert!(
        early > 0 && late > early * 3 / 2,
        "the mix changes over the sweep (zero crossings {early} early, {late} late)"
    );
    // Equal power: no quarter of the sweep dips to half the level of another.
    let (rms_early, rms_late) = (rms(&mono[..quarter]), rms(&mono[quarter * 2..quarter * 3]));
    assert!(
        rms_early > 0.5 * rms_late && rms_late > 0.5 * rms_early,
        "level holds across the crossfades ({rms_early} early, {rms_late} late)"
    );

    audio.despawn_patch(id);
}

// --------------------------------------------------------------------- voice loopback (§ 3b)

#[test]
fn voice_stream_loopback_plays_pushed_frames_with_low_latency() {
    let mut audio = headless();
    audio.start_voice_loopback(10).expect("loopback starts");
    let ramp: Arc<[f32]> = (0..960).map(|i| i as f32 / 960.0 * 0.8).collect();
    audio.push_voice_frames(ramp, Some(std::time::Instant::now()));
    let out = run(&mut audio, 0.15);

    let mono: Vec<f32> = out.chunks(2).map(|c| c[0]).collect();
    let peak = mono.iter().fold(0.0f32, |a, s| a.max(s.abs()));
    assert!(peak > 0.3, "ramp is audible, peak {peak}");

    let stats = audio.voice_stats().expect("stats");
    assert!(stats.chunks_pushed >= 1);
    assert!(stats.chunks_dropped == 0);
    assert!(
        stats.max_latency_ms < 60,
        "added loopback latency below 60 ms: {}",
        stats.max_latency_ms
    );
    audio.stop_voice_loopback();
    ticks(&mut audio, 2);
    assert!(
        audio.voice_stats().is_none(),
        "stopping clears the loopback handle"
    );
}

// --------------------------------------------------------------------- suspend/resume (§ 4)

#[test]
fn audio_survives_twenty_suspend_resume_cycles() {
    let mut audio = headless();
    for _ in 0..20 {
        audio.suspend_output();
        audio.resume_output();
    }
    assert_eq!(audio.suspend_count(), 20);
    audio
        .play(&SoundClip::new(vec![0.4; 96], RATE).expect("clip"))
        .expect("enqueue");
    audio.drain();
    let out = run(&mut audio, 0.05);
    assert!(
        out.iter().any(|s| s.abs() > 0.1),
        "audio still plays after 20 suspend cycles"
    );
    // The buses survived the rebuild.
    assert!(audio.bus_volume("Music").is_ok());
}

// --------------------------------------------------------------------- emitters orbit (sample basis)

#[test]
fn attached_emitters_track_the_listener() {
    use regia_ecs::ecs::entity::EntityIndex;
    use regia_ecs::prelude::*;
    let mut audio = headless();
    let entity = Entity::from_index(EntityIndex::from_raw_u32(7).expect("valid index"));
    audio.set_listener(ListenerFrame::looking_at(Vec3::ZERO, -Vec3::Z));
    let emitter = AudioEmitter {
        min_distance: 1.0,
        max_distance: 20.0,
        rolloff: RolloffCurve::Linear,
        air_absorption: true,
        ..AudioEmitter::default()
    };
    audio
        .start_emitter(entity, looping_tone(300.0, 0.3), &emitter, 0.0)
        .expect("emitter starts");
    assert_eq!(audio.real_voice_count(), 1);

    // Orbit: the emitter sweeps around the listener; gain and pan follow.
    for step in 0..120 {
        let angle = step as f32 / 120.0 * std::f32::consts::TAU;
        let position = Vec3::new(angle.cos() * 5.0, 0.0, angle.sin() * 5.0);
        audio.sync_emitters(&[(entity, emitter.clone(), position)]);
        audio.tick(FRAME);
        let _ = audio.render_offline((RATE as f64 * FRAME) as usize);
    }
    // The emitter voice is still alive and tracked.
    assert_eq!(audio.real_voice_count(), 1);
    audio.stop_emitter(entity, Duration::from_millis(50));
    ticks(&mut audio, 6);
    assert_eq!(audio.real_voice_count(), 0, "emitter stopped");
}
