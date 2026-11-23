// SPDX-License-Identifier: MPL-2.0
//! Propagation media end to end (`docs/specs/16-audio.md` § 3c): sounds rendered through the
//! offline mixer on Earth, Mars and airless worlds, through the ground and into interiors, with
//! the arrival times, levels and spectra measured on the output.

use std::time::Duration;

use regia_app::{App, TimeUpdateStrategy};
use regia_audio::{
    Audio, AudioEmitter, AudioInteriorVolume, AudioListener, AudioMedium, AudioPlugin, AudioPropagation,
    AudioSettings, EmitterCategory, InteriorFrame, ListenerContact, ListenerCoupling, ListenerFrame,
    PlaybackParams, SoundAsset, SoundClip,
};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_scene::Transform;
use regia_scene::level::{AtmospherePreset, LevelSettings};

const RATE: u32 = 48_000;
const FRAME: f64 = 1.0 / 60.0;
const FRAMES_PER_TICK: usize = 800;

fn headless(medium: AudioMedium) -> Audio {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    audio.set_medium(medium);
    // Face +X, where these tests place their sources: dead ahead, centred in both channels (the
    // measurements read the left channel).
    audio.set_listener(ListenerFrame::looking_at(Vec3::ZERO, Vec3::X));
    audio
}

/// A burst of equal-amplitude sines (5 ms attack, `seconds` long).
fn tones(freqs: &[f32], seconds: f32, amp: f32) -> SoundAsset {
    let len = (seconds * RATE as f32) as usize;
    let samples: Vec<f32> = (0..len)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            let attack = (t / 0.005).min(1.0);
            freqs
                .iter()
                .map(|f| (t * f * std::f32::consts::TAU).sin())
                .sum::<f32>()
                * amp
                * attack
        })
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("clip"))
}

fn far() -> AudioEmitter {
    AudioEmitter {
        min_distance: 1.0,
        max_distance: 5_000.0,
        ..AudioEmitter::default()
    }
}

fn floating() -> ListenerCoupling {
    ListenerCoupling {
        contact: ListenerContact::None,
        ..ListenerCoupling::default()
    }
}

/// Renders `seconds` (tick + render per 1/60 s), returning interleaved stereo.
fn run(audio: &mut Audio, seconds: f64) -> Vec<f32> {
    let mut out = Vec::new();
    for _ in 0..(seconds / FRAME).round() as usize {
        audio.tick(FRAME);
        out.extend_from_slice(&audio.render_offline(FRAMES_PER_TICK).expect("offline"));
    }
    out
}

fn left(frames: &[f32]) -> Vec<f32> {
    frames.iter().step_by(2).copied().collect()
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

/// Goertzel magnitude of `hz` (≈ a/2 for a sine of amplitude a).
fn goertzel(samples: &[f32], hz: f32) -> f32 {
    let k = std::f32::consts::TAU * hz / RATE as f32;
    let coeff = 2.0 * k.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() / samples.len().max(1) as f32
}

/// First time (s) the left channel exceeds `threshold`.
fn onset(frames: &[f32], threshold: f32) -> Option<f64> {
    left(frames)
        .iter()
        .position(|s| s.abs() > threshold)
        .map(|i| i as f64 / f64::from(RATE))
}

fn window(frames: &[f32], from_s: f64, to_s: f64) -> Vec<f32> {
    let l = left(frames);
    let a = ((from_s * f64::from(RATE)) as usize).min(l.len());
    let b = ((to_s * f64::from(RATE)) as usize).min(l.len());
    l[a..b].to_vec()
}

fn play(audio: &mut Audio, asset: &SoundAsset, at: Vec3, category: EmitterCategory, bus: &str) {
    audio
        .play_asset_propagated(
            asset,
            PlaybackParams::on_bus(bus),
            at,
            &far(),
            &AudioPropagation::for_category(category),
        )
        .expect("queued");
    audio.drain();
}

#[test]
fn airless_and_floating_is_silent() {
    let mut audio = headless(AudioMedium::airless());
    audio.set_listener_coupling(floating());
    run(&mut audio, 0.1); // the contact fade completes
    play(
        &mut audio,
        &tones(&[60.0, 800.0], 0.5, 0.4),
        Vec3::new(50.0, 0.0, 0.0),
        EmitterCategory::Explosion,
        "World",
    );
    let out = run(&mut audio, 1.5);
    assert!(rms(&out) < 1e-6, "silence expected, rms {}", rms(&out));
    assert!(
        audio.path_voices().is_empty(),
        "no path reaches a floating listener"
    );
}

#[test]
fn airless_grounded_on_ice_hears_a_delayed_low_passed_arrival() {
    let mut audio = headless(AudioMedium::airless());
    play(
        &mut audio,
        &tones(&[60.0, 2_000.0], 0.6, 0.4),
        Vec3::new(500.0, 0.0, 0.0),
        EmitterCategory::Explosion,
        "World",
    );
    let out = run(&mut audio, 1.5);
    let expected = 500.0 / 3_800.0;
    let t = onset(&out, 1e-5).expect("the ground wave arrives");
    assert!(
        (t - expected).abs() < 0.01,
        "arrival {t:.4} s vs r/v {expected:.4} s"
    );
    let body = window(&out, expected + 0.05, expected + 0.55);
    let low = goertzel(&body, 60.0);
    let high = goertzel(&body, 2_000.0);
    assert!(low > 1e-4, "audible: 60 Hz at {low}");
    assert!(high < low * 0.01, "low-passed: 2 kHz {high} vs 60 Hz {low}");
    assert!(audio.haptics().events_fired >= 1, "the arrival is felt");
}

#[test]
fn jumping_before_the_wave_arrives_silences_it() {
    let mut audio = headless(AudioMedium::airless());
    play(
        &mut audio,
        &tones(&[60.0], 0.6, 0.4),
        Vec3::new(500.0, 0.0, 0.0),
        EmitterCategory::Explosion,
        "World",
    );
    let early = run(&mut audio, 0.05);
    audio.set_listener_coupling(floating()); // mid-jump (low gravity) before the arrival at 132 ms
    let late = run(&mut audio, 1.0);
    assert!(rms(&early) < 1e-7);
    assert!(rms(&window(&late, 0.1, 1.0)) < 1e-6, "rms {}", rms(&late));
    assert!(audio.coupling_weight() == 0.0);
}

#[test]
fn landing_restores_the_ground_path() {
    let mut audio = headless(AudioMedium::airless());
    audio.set_listener_coupling(floating());
    run(&mut audio, 0.1);
    let hum = tones(&[70.0], 1.0, 0.4);
    play(
        &mut audio,
        &hum,
        Vec3::new(20.0, 0.0, 0.0),
        EmitterCategory::Machinery,
        "World",
    );
    assert!(audio.path_voices().is_empty());
    audio.set_listener_coupling(ListenerCoupling::default());
    run(&mut audio, 0.1);
    play(
        &mut audio,
        &hum,
        Vec3::new(20.0, 0.0, 0.0),
        EmitterCategory::Machinery,
        "World",
    );
    let out = run(&mut audio, 0.6);
    assert!(goertzel(&window(&out, 0.1, 0.6), 70.0) > 1e-4);
}

/// Level, arrival time and brightness of the same source at 200 m.
fn measure_at_200(medium: AudioMedium) -> (f64, f32, f32) {
    let mut audio = headless(medium);
    // Full level inside 50 m (a loud source), so Mars' −20 dB stays above the mixer's −48 dB
    // virtualisation threshold at 200 m.
    let loud = AudioEmitter {
        min_distance: 50.0,
        ..far()
    };
    audio
        .play_asset_propagated(
            &tones(&[150.0, 2_000.0], 0.8, 0.3),
            PlaybackParams::on_bus("World"),
            Vec3::new(200.0, 0.0, 0.0),
            &loud,
            &AudioPropagation::for_category(EmitterCategory::Projectile),
        )
        .expect("queued");
    audio.drain();
    let out = run(&mut audio, 2.0);
    let t = onset(&out, 1e-6).expect("arrives");
    let body = window(&out, t + 0.1, t + 0.7);
    let low = goertzel(&body, 150.0);
    let high = goertzel(&body, 2_000.0);
    (t, low, high / low.max(1e-12))
}

#[test]
fn mars_at_200_m_is_quieter_later_and_darker_than_earth() {
    let (te, le, be) = measure_at_200(AudioMedium::earth());
    let (tm, lm, bm) = measure_at_200(AudioMedium::mars());
    assert!((te - 200.0 / 343.3).abs() < 0.01, "earth arrival {te}");
    assert!((tm - 200.0 / 240.0).abs() < 0.01, "mars arrival {tm}");
    let drop_db = 20.0 * (lm / le).log10();
    assert!(
        (-23.0..-17.0).contains(&drop_db),
        "mars is ~20 dB quieter: {drop_db:.1} dB"
    );
    assert!(
        bm < be * 0.2,
        "mars is darker: 2k/150 ratio {bm:.4} vs earth {be:.4}"
    );
}

#[test]
fn an_explosion_on_earth_is_felt_through_the_ground_before_the_air_arrives() {
    let mut medium = AudioMedium::earth();
    medium.ground = "bedrock".into();
    let mut audio = headless(medium);
    // An explosion is loud: full level inside 30 m, so its air wave at 300 m (−20 dB) stays above
    // the mixer's −48 dB virtualisation threshold.
    let loud = AudioEmitter {
        min_distance: 30.0,
        ..far()
    };
    audio
        .play_asset_propagated(
            &tones(&[40.0, 1_000.0], 0.4, 0.4),
            PlaybackParams::on_bus("World"),
            Vec3::new(300.0, 0.0, 0.0),
            &loud,
            &AudioPropagation::for_category(EmitterCategory::Explosion),
        )
        .expect("queued");
    audio.drain();
    let ground_at = 300.0 / 5_000.0;
    let air_at = 300.0 / 343.3;
    let mut fired_before_air = 0;
    let mut out = Vec::new();
    for _ in 0..(1.5 / FRAME) as usize {
        audio.tick(FRAME);
        out.extend_from_slice(&audio.render_offline(FRAMES_PER_TICK).expect("offline"));
        if (out.len() / 2) as f64 / f64::from(RATE) < air_at {
            fired_before_air = audio.haptics().events_fired;
        }
    }
    let t = onset(&out, 1e-5).expect("arrives");
    assert!(
        (t - ground_at).abs() < 0.01,
        "first arrival through the ground at {t}"
    );
    let between = window(&out, ground_at + 0.02, air_at - 0.02);
    assert!(
        goertzel(&between, 40.0) > 1e-5,
        "the ground wave is heard before the air wave"
    );
    assert!(
        fired_before_air >= 1,
        "the rumble fires before the airborne arrival"
    );
    let after = window(&out, air_at + 0.02, air_at + 0.3);
    let dark = goertzel(&between, 1_000.0) / goertzel(&between, 40.0).max(1e-12);
    let bright = goertzel(&after, 1_000.0) / goertzel(&after, 40.0).max(1e-12);
    assert!(goertzel(&after, 1_000.0) > 1e-4, "then the air wave arrives");
    assert!(
        dark < 0.1 * bright,
        "the ground wave is dark ({dark:.4}), the air wave bright ({bright:.4})"
    );
}

fn hab() -> InteriorFrame {
    InteriorFrame {
        entity: Entity::from_raw_u32(42).expect("entity"),
        center: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        volume: AudioInteriorVolume {
            half_extents: Vec3::new(6.0, 3.0, 6.0),
            pressurised: true,
            wall: "steel".into(),
        },
    }
}

#[test]
fn a_pressurised_interior_has_its_own_air_and_exterior_impacts_come_through_the_structure() {
    let mut audio = headless(AudioMedium::airless());
    audio.set_interiors(vec![hab()]);
    audio.set_listener_coupling(floating());
    run(&mut audio, 0.1);
    // A machine inside the hab: airborne in the hab's air (3 m: ~9 ms), bright.
    play(
        &mut audio,
        &tones(&[220.0, 3_000.0], 0.5, 0.3),
        Vec3::new(3.0, 0.0, 0.0),
        EmitterCategory::Machinery,
        "World",
    );
    let inside = run(&mut audio, 0.6);
    let t = onset(&inside, 1e-5).expect("heard inside");
    // The floor (steel, 5.9 km/s) carries the machine's vibration before the air does.
    assert!(t <= 3.0 / 343.3 + 0.002, "first arrival {t}");
    let solved = audio.solve_propagation(
        &regia_audio::EmitterFrame {
            position: Vec3::new(3.0, 0.0, 0.0),
            ..Default::default()
        },
        &far(),
        &AudioPropagation::for_category(EmitterCategory::Machinery),
        "World",
    );
    assert_eq!(solved.air, Some(regia_audio::MediumKind::Interior));
    let air = solved.airborne.expect("the hab's own air");
    assert!((air.delay_secs - 3.0 / 343.3).abs() < 1e-4);
    assert!(solved.ground.expect("through the floor").delay_secs < air.delay_secs);
    assert!(
        goertzel(&window(&inside, 0.05, 0.45), 3_000.0) > 1e-3,
        "interior air is not muffled"
    );
    run(&mut audio, 0.5);
    // An impact outside on the ice: no air outside, it reaches the floating listener through the
    // walls, muffled.
    play(
        &mut audio,
        &tones(&[60.0, 3_000.0], 0.5, 0.4),
        Vec3::new(40.0, 0.0, 0.0),
        EmitterCategory::Impact,
        "World",
    );
    let outside = run(&mut audio, 0.6);
    let body = window(&outside, 0.05, 0.5);
    let low = goertzel(&body, 60.0);
    assert!(low > 1e-5, "the exterior impact is heard: {low}");
    assert!(goertzel(&body, 3_000.0) < low * 0.01, "…muffled");
}

#[test]
fn europa_pilot_on_foot_hears_only_the_suit() {
    // DO27 fixture: airless Europa, the pilot standing. Suit (breathing), radio and ground
    // thumps through the boots are heard; a hovering hull and a projectile in flight are not.
    let mut audio = headless(AudioMedium::airless());
    let breathing = tones(&[330.0], 0.8, 0.2);
    let radio = tones(&[1_200.0], 0.8, 0.2);
    let landing = tones(&[50.0], 0.8, 0.4);
    let hover = tones(&[700.0], 0.8, 0.4);
    let projectile = tones(&[2_500.0], 0.8, 0.4);
    play(
        &mut audio,
        &breathing,
        Vec3::new(0.0, 0.1, 0.0),
        EmitterCategory::Body,
        "World",
    );
    play(
        &mut audio,
        &radio,
        Vec3::new(0.0, 0.0, -1.0),
        EmitterCategory::Radio,
        "Voice",
    );
    play(
        &mut audio,
        &landing,
        Vec3::new(15.0, 0.0, 0.0),
        EmitterCategory::Landing,
        "World",
    );
    let mut hovering = AudioPropagation::for_category(EmitterCategory::HullHover);
    hovering.height_above_ground = 3.0;
    audio
        .play_asset_propagated(
            &hover,
            PlaybackParams::on_bus("World"),
            Vec3::new(30.0, 3.0, 0.0),
            &far(),
            &hovering,
        )
        .expect("queued");
    play(
        &mut audio,
        &projectile,
        Vec3::new(10.0, 2.0, 0.0),
        EmitterCategory::Projectile,
        "World",
    );
    audio.drain();
    let out = run(&mut audio, 0.8);
    let body = window(&out, 0.1, 0.7);
    assert!(goertzel(&body, 330.0) > 1e-3, "breathing");
    assert!(goertzel(&body, 1_200.0) > 1e-3, "radio");
    assert!(goertzel(&body, 50.0) > 1e-4, "a landing thumps through the boots");
    assert!(goertzel(&body, 700.0) < 1e-6, "a hull hovering 3 m up is silent");
    assert!(
        goertzel(&body, 2_500.0) < 1e-6,
        "a projectile in flight is silent"
    );
    // A hull hovering within a metre of the ice couples weakly.
    let mut skimming = AudioPropagation::for_category(EmitterCategory::HullHover);
    skimming.height_above_ground = 0.4;
    let prop = audio.solve_propagation(
        &regia_audio::EmitterFrame {
            position: Vec3::new(30.0, 0.4, 0.0),
            ..Default::default()
        },
        &far(),
        &skimming,
        "World",
    );
    assert!(
        prop.ground.is_some_and(|g| g.audible()),
        "weak coupling near the ground"
    );
}

fn moving_emitter_pitch(preset: AtmospherePreset) -> f32 {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(FRAME)));
    app.insert_resource(LevelSettings {
        atmosphere: regia_scene::level::LevelAtmosphere {
            preset,
            ..Default::default()
        },
        ..Default::default()
    });
    app.add_plugins(AudioPlugin);
    app.world_mut()
        .spawn((AudioListener::default(), Transform::default()));
    let emitter = AudioEmitter {
        doppler_factor: 1.0,
        ..far()
    };
    let entity = app
        .world_mut()
        .spawn((
            emitter.clone(),
            Transform::from_translation(Vec3::new(-200.0, 0.0, 0.0)),
        ))
        .id();
    app.update();
    let asset = tones(&[200.0], 0.5, 0.2);
    app.world_mut()
        .resource_mut::<Audio>()
        .start_emitter(entity, asset, &emitter, 0.0)
        .expect("emitter");
    // Approach at 1 m per 60 Hz frame = 60 m/s, long enough for the velocity estimate's 50 ms
    // smoothing to settle (20 frames: within 0.2 %).
    for _ in 0..20 {
        if let Some(mut t) = app.world_mut().get_mut::<Transform>(entity) {
            t.translation.x += 1.0;
        }
        app.update();
    }
    app.world()
        .resource::<Audio>()
        .emitter_propagation(entity)
        .and_then(|p| p.airborne)
        .map_or(f32::NAN, |a| a.pitch_ratio)
}

#[test]
fn doppler_uses_the_level_medium_speed_of_sound() {
    let earth = moving_emitter_pitch(AtmospherePreset::Earth);
    let mars = moving_emitter_pitch(AtmospherePreset::Mars);
    assert!((earth - 343.3 / 283.3).abs() < 0.01, "earth {earth}");
    assert!((mars - 240.0 / 180.0).abs() < 0.01, "mars {mars}");
}

#[test]
fn attached_emitter_ground_voice_follows_contact() {
    // A deployed drill on the ice 30 m away (looping, coupled): heard through the boots,
    // gone 50 ms after the pilot jumps, back on landing.
    let mut audio = headless(AudioMedium::airless());
    let entity = Entity::from_raw_u32(9).expect("entity");
    audio.set_emitter_propagation(entity, AudioPropagation::for_category(EmitterCategory::Machinery));
    let drill = tones(&[80.0], 0.5, 0.4);
    let emitter = AudioEmitter {
        bus: Some("World".into()),
        ..far()
    };
    audio
        .start_emitter(entity, drill, &emitter, 0.0)
        .expect("emitter");
    let sync =
        |audio: &mut Audio| audio.sync_emitters(&[(entity, emitter.clone(), Vec3::new(30.0, 0.0, 0.0))]);
    let mut out = Vec::new();
    for _ in 0..30 {
        sync(&mut audio);
        audio.tick(FRAME);
        out.extend(audio.render_offline(FRAMES_PER_TICK).expect("offline"));
    }
    let standing = goertzel(&window(&out, 0.2, 0.5), 80.0);
    assert!(standing > 1e-4, "the drill is felt through the boots: {standing}");
    audio.set_listener_coupling(floating());
    let mut out = Vec::new();
    for _ in 0..30 {
        sync(&mut audio);
        audio.tick(FRAME);
        out.extend(audio.render_offline(FRAMES_PER_TICK).expect("offline"));
    }
    let jumping = goertzel(&window(&out, 0.15, 0.5), 80.0);
    assert!(
        jumping < standing * 1e-3,
        "airborne pilot: {jumping} vs {standing}"
    );
    // The emitter's looping voice is an emitter: its ground voice exists while it lives.
    assert!(audio.path_voices().iter().any(|(role, _)| *role == "ground"));
}
