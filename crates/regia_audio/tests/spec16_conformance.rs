// SPDX-License-Identifier: MPL-2.0
//! Spec 16 conformance (lane sprint-spec-06-16-audio, ADR-5614): tests for behavior the code
//! already had but no test pinned. Cue cooldown and the `Quietest` steal policy, live voice
//! volume and pan changes, the emitter defaults, the listener coupling and interior constants,
//! the medium-bypass buses and the ground coda floor.
//!
//! Headless offline mixer, static clips, fixed frame steps; no wall clock.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use regia_audio::{
    Audio, AudioEmitter, AudioSettings, BusLayout, CueEntry, GroundMaterial, INTERIOR_RADIATION,
    ListenerContact, MIN_CODA_SECS, RockField, RockTuning, SoundAsset, SoundClip, SoundCue, StealPolicy,
    WALL_TRANSMISSION, ZoneField, rock_thickness,
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

/// A sine clip of `seconds`.
fn tone(hz: f32, seconds: f64, amp: f32) -> SoundAsset {
    let len = (seconds * f64::from(RATE)) as usize;
    let samples: Vec<f32> = (0..len)
        .map(|i| ((i as f32 / RATE as f32) * hz * std::f32::consts::TAU).sin() * amp)
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

fn channel_rms(frames: &[f32], channel: usize) -> f32 {
    let samples: Vec<f32> = frames.iter().skip(channel).step_by(2).copied().collect();
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

fn single_cue(name: &str, asset: SoundAsset) -> SoundCue {
    let mut cue = SoundCue::new(name, vec![CueEntry { asset, weight: 1.0 }]);
    cue.max_concurrent = None;
    cue
}

/// Spec 16 § 2: a cue's cooldown refuses plays inside the window and allows them after it.
#[test]
fn a_cue_cooldown_refuses_plays_inside_the_window() {
    let mut audio = headless();
    let mut cue = single_cue("hit", tone(500.0, 0.05, 0.2));
    cue.cooldown = Duration::from_millis(100);
    audio.register_cue(cue);
    assert!(audio.play_cue("hit").expect("registered").is_some(), "first play");
    assert!(
        audio.play_cue("hit").expect("registered").is_none(),
        "a second play at once is inside the cooldown"
    );
    // 3 frames = 50 ms: still cooling down.
    let _ = run(&mut audio, 3);
    assert!(audio.play_cue("hit").expect("registered").is_none(), "50 ms in");
    // 4 more frames: 116 ms after the first play.
    let _ = run(&mut audio, 4);
    assert!(
        audio.play_cue("hit").expect("registered").is_some(),
        "the cooldown is over after 100 ms"
    );
}

/// Spec 16 § 2: at `max_concurrent` with `StealPolicy::Quietest` the quietest live voice of the
/// cue is stolen, even when it is not the oldest.
#[test]
fn the_quietest_steal_policy_stops_the_quietest_voice_not_the_oldest() {
    let mut audio = headless();
    let asset = tone(300.0, 2.0, 0.2);
    let mut cue = single_cue("shot", asset.clone());
    cue.max_concurrent = Some(2);
    cue.steal = StealPolicy::Quietest;
    cue.volume_db = 0.0;
    audio.register_cue(cue.clone());
    let loud = audio.play_cue("shot").expect("registered").expect("loud plays");
    let _ = run(&mut audio, 2);
    // The same cue at −30 dB: the second (newer) voice is the quiet one.
    cue.volume_db = -30.0;
    audio.register_cue(cue.clone());
    let quiet = audio.play_cue("shot").expect("registered").expect("quiet plays");
    let _ = run(&mut audio, 2);
    cue.volume_db = 0.0;
    audio.register_cue(cue);
    let third = audio.play_cue("shot").expect("registered").expect("third steals");
    let _ = run(&mut audio, 3);
    assert!(audio.voice_playing(loud), "the older, louder voice keeps playing");
    assert!(!audio.voice_playing(quiet), "the quieter voice was stolen");
    assert!(audio.voice_playing(third));
}

/// Spec 16 § 2: `set_volume(tween)` and `set_pan(tween)` on a playing voice change what is heard.
#[test]
fn voice_volume_and_pan_changes_are_heard() {
    let mut audio = headless();
    audio.register_cue(single_cue("hum", tone(220.0, 3.0, 0.3)));
    let id = audio.play_cue("hum").expect("registered").expect("plays");
    let _ = run(&mut audio, 3);
    let before = run(&mut audio, 6);
    let level = (channel_rms(&before, 0) + channel_rms(&before, 1)) / 2.0;
    assert!(level > 0.05, "audible: {level}");

    audio.set_voice_volume_db(id, -12.0, 0.0);
    let _ = run(&mut audio, 2);
    let after = run(&mut audio, 6);
    let quieter = (channel_rms(&after, 0) + channel_rms(&after, 1)) / 2.0;
    let change_db = 20.0 * (quieter / level).log10();
    assert!(
        (change_db + 12.0).abs() < 1.0,
        "volume −12 dB measured {change_db} dB"
    );

    audio.set_voice_volume_db(id, 0.0, 0.0);
    audio.set_voice_pan(id, -1.0, 0.0);
    let _ = run(&mut audio, 2);
    let panned = run(&mut audio, 6);
    let (left, right) = (channel_rms(&panned, 0), channel_rms(&panned, 1));
    assert!(left > 0.05, "the left ear hears it: {left}");
    assert!(left > right * 10.0, "hard left: left {left}, right {right}");
}

/// Spec 16 § 3: emitter defaults — Doppler factor 0, no rolloff fade, fully 3D.
#[test]
fn emitter_defaults_are_the_spec_values() {
    let emitter = AudioEmitter::default();
    assert_eq!(emitter.doppler_factor, 0.0);
    assert_eq!(emitter.fade_start, None);
    assert_eq!(emitter.spatial_blend, 1.0);
}

/// Spec 16 § 3c listener coupling: Boots 500 Hz −6 dB, Vehicle 250 Hz −3 dB, Structure 700 Hz
/// −9 dB, None uncoupled; interiors: wall −35 dB / 400 Hz, radiation −18 dB / 300 Hz.
#[test]
fn listener_coupling_and_interior_filters_are_the_spec_values() {
    for (contact, cutoff, gain) in [
        (ListenerContact::Boots, 500.0, -6.0),
        (ListenerContact::Vehicle, 250.0, -3.0),
        (ListenerContact::Structure, 700.0, -9.0),
    ] {
        let filter = contact.filter().expect("coupled");
        assert_eq!((filter.cutoff_hz, filter.gain_db), (cutoff, gain), "{contact:?}");
        assert!(contact.coupled());
    }
    assert_eq!(ListenerContact::None.filter(), None);
    assert_eq!(
        (WALL_TRANSMISSION.cutoff_hz, WALL_TRANSMISSION.gain_db),
        (400.0, -35.0)
    );
    assert_eq!(
        (INTERIOR_RADIATION.cutoff_hz, INTERIOR_RADIATION.gain_db),
        (300.0, -18.0)
    );
}

/// Spec 16 § 3c bypass: by default `Music`, `Ui`, `Voice` and their children skip the medium;
/// `Master`, `Sfx`, `World` and `Ambience` do not.
#[test]
fn the_default_medium_bypass_buses_are_music_ui_and_voice() {
    let layout = BusLayout::default_tree();
    for bus in ["Music", "Ui", "Voice"] {
        assert!(layout.bypasses_medium(bus), "{bus} bypasses");
    }
    for bus in ["Master", "Sfx", "World", "Ambience", "NoSuchBus"] {
        assert!(!layout.bypasses_medium(bus), "{bus} goes through the medium");
    }
}

/// Spec 16 § 3c: the coda's 60 dB decay is clamped to 0.15..=8 s; a very low-Q ground sits on
/// the floor.
#[test]
fn a_low_q_ground_coda_sits_on_the_floor() {
    assert_eq!(MIN_CODA_SECS, 0.15);
    let mut ground = GroundMaterial::presets()
        .into_iter()
        .next()
        .expect("at least one preset");
    ground.q = 0.5;
    assert_eq!(ground.coda_secs(), MIN_CODA_SECS);
    // ln(1000)·Q/(π·100 Hz) at Q 50 is 1.1 s: inside the clamp, so the formula shows through.
    ground.q = 50.0;
    let expected = 1000f32.ln() * 50.0 / (std::f32::consts::PI * 100.0);
    assert!(
        (ground.coda_secs() - expected).abs() < 1e-4,
        "{}",
        ground.coda_secs()
    );
}

/// Solid rock with its surface at y = 1000, counting how often it is asked.
struct CountingRock(AtomicUsize);

impl RockField for CountingRock {
    fn surface_height(&self, _x: f32, _z: f32) -> Option<f32> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Some(1000.0)
    }
}

/// Spec 16 § 3d: rock thickness marches at a 1 m step, at most 256 samples per ray; a longer ray
/// takes a coarser step and still measures its whole length.
#[test]
fn the_rock_march_takes_at_most_256_samples() {
    let tuning = RockTuning::default();
    assert_eq!((tuning.sample_step_m, tuning.max_samples), (1.0, 256));
    let zones = ZoneField::default();
    let short = CountingRock(AtomicUsize::new(0));
    let metres = rock_thickness(&short, &zones, &tuning, Vec3::ZERO, Vec3::new(40.0, 0.0, 0.0));
    assert_eq!(short.0.load(Ordering::Relaxed), 40, "1 m step on a 40 m ray");
    assert!((metres - 40.0).abs() < 1e-3, "{metres}");
    let long = CountingRock(AtomicUsize::new(0));
    let metres = rock_thickness(&long, &zones, &tuning, Vec3::ZERO, Vec3::new(1000.0, 0.0, 0.0));
    assert_eq!(long.0.load(Ordering::Relaxed), 256, "capped");
    assert!((metres - 1000.0).abs() < 0.01, "{metres}");
}

/// 120 bpm, 4/4: a bar is 96 000 samples at 48 kHz.
const BAR: usize = 96_000;

fn constant(value: f32, frames: usize) -> SoundAsset {
    SoundAsset::from_clip(&SoundClip::new(vec![value; frames], RATE).expect("a valid clip"))
}

/// Spec 16 § 3a: a stinger can wait for the end of the segment. With a 2-bar segment, a stinger
/// asked for during bar 0 starts on the exact sample 2 bars in.
#[test]
fn a_stinger_can_land_on_the_end_of_the_segment() {
    let mut buses = BusLayout::default_tree();
    for bus in &mut buses.buses {
        if bus.name == "Music" {
            bus.volume_db = 0.0;
        }
    }
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        buses,
        ..AudioSettings::default()
    });
    let mut asset = regia_audio::MusicAsset::from_stems(
        "t",
        120.0,
        4,
        vec![regia_audio::MusicStem {
            name: "a".into(),
            asset: constant(0.0, BAR),
            base_db: 0.0,
            intensity: Vec::new(),
        }],
    );
    asset.segment_bars = Some(2);
    asset.stingers.push(regia_audio::MusicStinger {
        name: "hit".into(),
        asset: constant(0.5, 2_000),
        volume_db: 0.0,
        sync: regia_audio::MusicSync::EndOfSegment,
    });
    audio.play_music(&asset, Duration::ZERO).expect("plays");
    let step = 800usize;
    let mut heard: Vec<f32> = Vec::new();
    let render = |audio: &mut Audio, frames: usize, heard: &mut Vec<f32>| {
        let target = heard.len() + frames;
        while heard.len() < target {
            audio.tick(step as f64 / f64::from(RATE));
            let block = audio.render_offline(step).expect("the offline backend");
            heard.extend(block.iter().step_by(2).copied());
        }
    };
    render(&mut audio, BAR / 2, &mut heard);
    let schedule = audio.music_stinger("hit", None).expect("schedules");
    assert_eq!(schedule.at_frame, 2 * BAR as u64);
    render(&mut audio, 2 * BAR, &mut heard);
    assert_eq!(heard.iter().position(|s| *s >= 0.25), Some(2 * BAR));
}

/// Spec 16 § 3f: the whine follows its rpm gain curve and is pitched `rpm / recorded rpm`; wind
/// follows its speed gain curve and rises in pitch with speed (steady state through `settle`).
#[test]
fn vehicle_whine_and_wind_follow_their_curves() {
    use regia_audio::{VehicleAudioDef, VehicleAudioInputs, VehicleAudioModel, VehicleVoiceRole, db_to_gain};
    let def = VehicleAudioDef::placeholder();
    let whine = def.whine.clone().expect("the placeholder has a whine");
    let wind = def.wind.clone().expect("the placeholder has wind");
    let mut model = VehicleAudioModel::new(def, 4).expect("valid placeholder");
    let voice = |model: &VehicleAudioModel, role: VehicleVoiceRole| {
        let v = model
            .voices()
            .iter()
            .find(|v| v.role == role)
            .expect("the voice exists");
        (v.amplitude, v.pitch)
    };
    let inputs = |rpm: f32, speed: f32| VehicleAudioInputs {
        rpm,
        throttle: 1.0,
        engine_torque: 400.0,
        speed,
        ..VehicleAudioInputs::default()
    };
    // 4000 rpm is a point of the placeholder's whine curve (−24 dB).
    model.settle(&inputs(4000.0, 0.0));
    let params = model.params();
    let share = whine.off_load_share + (1.0 - whine.off_load_share) * params.load;
    let (amplitude, pitch) = voice(&model, VehicleVoiceRole::Whine);
    let expected = db_to_gain(-24.0) * share * params.limiter;
    assert!(
        (amplitude - expected).abs() < 1e-4,
        "whine {amplitude}, expected {expected}"
    );
    assert!(
        (pitch - 4000.0 / whine.recorded_rpm).abs() < 1e-4,
        "whine pitch {pitch}"
    );
    // Higher rpm: louder whine (the curve rises to −16 dB at 7000).
    model.settle(&inputs(6000.0, 0.0));
    let (louder, higher) = voice(&model, VehicleVoiceRole::Whine);
    assert!(
        louder > amplitude && higher > pitch,
        "6000 rpm: {louder} at {higher}"
    );

    // Wind: about −100 dB at rest, the curve's −14 dB at 40 m/s, pitch rising with speed.
    model.settle(&inputs(3000.0, 0.0));
    let (still, still_pitch) = voice(&model, VehicleVoiceRole::Wind);
    assert!(still < 1e-4, "no wind at rest: {still}");
    model.settle(&inputs(3000.0, 40.0));
    let (fast, fast_pitch) = voice(&model, VehicleVoiceRole::Wind);
    assert!((fast - db_to_gain(-14.0)).abs() < 1e-4, "wind at 40 m/s: {fast}");
    let expected_pitch = (1.0 + 40.0 * wind.pitch_per_mps).clamp(0.5, 2.0);
    assert!(
        (fast_pitch - expected_pitch).abs() < 1e-4,
        "wind pitch {fast_pitch}"
    );
    assert!(fast_pitch > still_pitch || wind.pitch_per_mps == 0.0);
}
