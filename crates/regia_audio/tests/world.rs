// SPDX-License-Identifier: MPL-2.0
//! Entity-driven spatial audio, end to end (`docs/specs/16-audio.md` § 3): an `App` with the
//! scene and audio plugins, entities with `AudioListener`/`AudioEmitter`/`AudioReverbZone`
//! components, and every assertion on samples rendered by the offline backend — no device, no
//! direct calls into the mixer.

use std::time::Duration;

use regia_app::{App, TimeUpdateStrategy};
use regia_audio::{
    Audio, AudioEmitter, AudioListener, AudioOcclusion, AudioPlugin, AudioReverbZone, AudioSettings,
    RolloffCurve, SPEED_OF_SOUND, SoundAsset, SoundAssets, SoundClip,
};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_scene::{ScenePlugin, Transform};

const RATE: u32 = 48_000;

fn app(dt: f64) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(dt)));
    app.add_plugins((ScenePlugin, AudioPlugin));
    app
}

fn tone(hz: f32, seconds: f64, amp: f32) -> SoundAsset {
    let len = (seconds * f64::from(RATE)) as usize;
    let samples: Vec<f32> = (0..len)
        .map(|i| ((i as f32 / RATE as f32) * hz * std::f32::consts::TAU).sin() * amp)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("tone"))
}

/// Deterministic white noise (LCG), for broadband measurements.
fn noise(seconds: f64, amp: f32) -> SoundAsset {
    let len = (seconds * f64::from(RATE)) as usize;
    let mut state = 0x1234_5678u32;
    let samples: Vec<f32> = (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp
        })
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("noise"))
}

fn add_sound(app: &mut App, name: &str, asset: SoundAsset) {
    app.world_mut().resource_mut::<SoundAssets>().add(name, asset);
}

/// An emitter with a flat rolloff out to `max` metres, playing `sound` (by name).
fn emitter(sound: &str, max: f32) -> AudioEmitter {
    AudioEmitter {
        sound_name: sound.into(),
        min_distance: max,
        max_distance: max * 2.0,
        rolloff: RolloffCurve::Inverse,
        occlusion: false,
        ..AudioEmitter::default()
    }
}

/// Runs `frames` app updates, rendering each frame's audio; returns interleaved stereo.
fn run(app: &mut App, frames: usize, dt: f64) -> Vec<f32> {
    let per = (f64::from(RATE) * dt).round() as usize;
    let mut out = Vec::with_capacity(frames * per * 2);
    for _ in 0..frames {
        app.update();
        let rendered = app
            .world_mut()
            .resource_mut::<Audio>()
            .render_offline(per)
            .expect("offline render");
        out.extend_from_slice(&rendered);
    }
    out
}

fn channels(frames: &[f32]) -> (Vec<f32>, Vec<f32>) {
    (
        frames.iter().step_by(2).copied().collect(),
        frames.iter().skip(1).step_by(2).copied().collect(),
    )
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

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

/// The strongest frequency in `lo..hi` (1 Hz resolution) of the left channel.
fn dominant_hz(frames: &[f32], lo: f32, hi: f32) -> f32 {
    let (left, _) = channels(frames);
    let mut best = (lo, 0.0f32);
    let mut hz = lo;
    while hz <= hi {
        let level = goertzel(&left, hz);
        if level > best.1 {
            best = (hz, level);
        }
        hz += 1.0;
    }
    best.0
}

/// The last `seconds` of interleaved stereo.
fn tail(frames: &[f32], seconds: f64) -> &[f32] {
    let n = (seconds * f64::from(RATE)) as usize * 2;
    &frames[frames.len().saturating_sub(n)..]
}

fn listener_at(app: &mut App, position: Vec3, rotation: Quat, hrtf: bool) -> Entity {
    app.world_mut()
        .spawn((
            AudioListener { hrtf },
            Transform {
                translation: position,
                rotation,
                ..Default::default()
            },
        ))
        .id()
}

// ------------------------------------------------------------------------------- placement

#[test]
fn a_parented_emitter_is_heard_where_it_is_drawn() {
    let dt = 1.0 / 60.0;
    let mut app = app(dt);
    add_sound(&mut app, "hum", tone(300.0, 0.5, 0.3));
    listener_at(&mut app, Vec3::ZERO, Quat::IDENTITY, false);
    // The emitter's local transform is the identity; its parent stands 5 m to the right.
    let parent = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::X * 5.0))
        .id();
    app.world_mut()
        .spawn((emitter("hum", 20.0), Transform::default(), ChildOf(parent)));
    let out = run(&mut app, 30, dt);
    let (l, r) = channels(tail(&out, 0.2));
    assert!(rms(&r) > 2.0 * rms(&l), "right {} left {}", rms(&r), rms(&l));
}

#[test]
fn a_parented_listener_turns_with_its_parent() {
    let dt = 1.0 / 60.0;
    let mut app = app(dt);
    add_sound(&mut app, "hum", tone(300.0, 0.5, 0.3));
    // The listener's parent is turned 180°: the listener faces +Z, so +X is on its left.
    let rig = app
        .world_mut()
        .spawn(Transform {
            rotation: Quat::from_rotation_y(std::f32::consts::PI),
            ..Default::default()
        })
        .id();
    app.world_mut()
        .spawn((AudioListener::default(), Transform::default(), ChildOf(rig)));
    app.world_mut()
        .spawn((emitter("hum", 20.0), Transform::from_translation(Vec3::X * 5.0)));
    let out = run(&mut app, 30, dt);
    let (l, r) = channels(tail(&out, 0.2));
    assert!(rms(&l) > 2.0 * rms(&r), "left {} right {}", rms(&l), rms(&r));
}

#[test]
fn spawning_an_emitter_plays_it_and_despawning_stops_it() {
    let dt = 1.0 / 60.0;
    let mut app = app(dt);
    let hum = app
        .world_mut()
        .resource_mut::<SoundAssets>()
        .add("hum", tone(440.0, 0.5, 0.3));
    listener_at(&mut app, Vec3::ZERO, Quat::IDENTITY, false);
    // By handle this time; no code beyond the spawn.
    let entity = app
        .world_mut()
        .spawn((
            AudioEmitter {
                occlusion: false,
                ..AudioEmitter::new(hum)
            },
            Transform::from_translation(Vec3::NEG_Z * 2.0),
        ))
        .id();
    let playing = run(&mut app, 20, dt);
    assert_eq!(app.world().resource::<Audio>().real_voice_count(), 1);
    assert!(rms(tail(&playing, 0.1)) > 0.05, "the emitter is audible");
    // It loops past the clip's 0.5 s.
    let later = run(&mut app, 40, dt);
    assert!(rms(tail(&later, 0.1)) > 0.05, "still looping after 1 s");

    app.world_mut().despawn(entity);
    let after = run(&mut app, 20, dt);
    assert_eq!(app.world().resource::<Audio>().real_voice_count(), 0);
    assert!(rms(tail(&after, 0.1)) < 1e-4, "silent after despawn");
}

#[test]
fn a_one_shot_emitter_plays_once_and_a_new_sound_restarts_it() {
    let dt = 1.0 / 60.0;
    let mut app = app(dt);
    add_sound(&mut app, "blip", tone(600.0, 0.2, 0.3));
    add_sound(&mut app, "boop", tone(900.0, 2.0, 0.3));
    listener_at(&mut app, Vec3::ZERO, Quat::IDENTITY, false);
    let entity = app
        .world_mut()
        .spawn((
            AudioEmitter {
                looping: false,
                ..emitter("blip", 20.0)
            },
            Transform::from_translation(Vec3::NEG_Z),
        ))
        .id();
    let out = run(&mut app, 45, dt);
    assert!(goertzel(&channels(&out[..9_600]).0, 600.0) > 0.01, "played");
    assert!(rms(tail(&out, 0.2)) < 1e-4, "played once, not looped");
    // Changing the sound restarts the emitter with the new one.
    app.world_mut()
        .get_mut::<AudioEmitter>(entity)
        .expect("emitter")
        .sound_name = "boop".into();
    let out = run(&mut app, 20, dt);
    let (l, _) = channels(tail(&out, 0.15));
    assert!(goertzel(&l, 900.0) > 0.01, "the new sound plays");
}

// ------------------------------------------------------------------------------- Doppler

/// Radial speed for a 1.111× Doppler shift with factor 1: `c (1 − 1/1.111)`.
const APPROACH: f32 = SPEED_OF_SOUND * 0.1;

#[test]
fn doppler_uses_the_real_frame_delta() {
    // 30 fps frames: the old fixed `× 60` estimate would double the speed (1250 Hz).
    let dt = 1.0 / 30.0;
    let mut app = app(dt);
    add_sound(&mut app, "tone", tone(1_000.0, 0.5, 0.3));
    listener_at(&mut app, Vec3::ZERO, Quat::IDENTITY, false);
    let start = Vec3::NEG_Z * 80.0;
    let entity = app
        .world_mut()
        .spawn((
            AudioEmitter {
                doppler_factor: 1.0,
                ..emitter("tone", 100.0)
            },
            Transform::from_translation(start),
        ))
        .id();
    let mut out = Vec::new();
    for frame in 0..30 {
        let position = start + Vec3::Z * APPROACH * (frame as f32 * dt as f32);
        app.world_mut()
            .get_mut::<Transform>(entity)
            .expect("transform")
            .translation = position;
        out.extend(run(&mut app, 1, dt));
    }
    let expected = 1_000.0 * SPEED_OF_SOUND / (SPEED_OF_SOUND - APPROACH);
    let measured = dominant_hz(tail(&out, 0.4), 900.0, 1_400.0);
    assert!(
        (measured - expected).abs() < 8.0,
        "measured {measured} Hz, expected {expected}"
    );
}

#[test]
fn a_moving_listener_hears_doppler() {
    let dt = 1.0 / 60.0;
    let mut app = app(dt);
    add_sound(&mut app, "tone", tone(1_000.0, 0.5, 0.3));
    let listener = listener_at(&mut app, Vec3::ZERO, Quat::IDENTITY, false);
    app.world_mut().spawn((
        AudioEmitter {
            doppler_factor: 1.0,
            ..emitter("tone", 200.0)
        },
        Transform::from_translation(Vec3::NEG_Z * 150.0),
    ));
    let mut out = Vec::new();
    for frame in 0..60 {
        app.world_mut()
            .get_mut::<Transform>(listener)
            .expect("transform")
            .translation = Vec3::NEG_Z * APPROACH * (frame as f32 * dt as f32);
        out.extend(run(&mut app, 1, dt));
    }
    // Listener approaching a static source: (c + v) / c.
    let expected = 1_000.0 * (SPEED_OF_SOUND + APPROACH) / SPEED_OF_SOUND;
    let measured = dominant_hz(tail(&out, 0.4), 900.0, 1_400.0);
    assert!(
        (measured - expected).abs() < 8.0,
        "measured {measured} Hz, expected {expected}"
    );
}

// ------------------------------------------------------------------------------- cones

#[test]
fn an_emitter_cone_follows_its_rotation() {
    let level = |rotation: Quat| {
        let dt = 1.0 / 60.0;
        let mut app = app(dt);
        add_sound(&mut app, "hum", tone(300.0, 0.5, 0.3));
        listener_at(&mut app, Vec3::ZERO, Quat::IDENTITY, false);
        app.world_mut().spawn((
            AudioEmitter {
                cone_inner_degrees: 30.0,
                cone_outer_degrees: 60.0,
                cone_outer_gain: 0.1,
                ..emitter("hum", 20.0)
            },
            Transform {
                translation: Vec3::NEG_Z * 5.0,
                rotation,
                ..Default::default()
            },
        ));
        rms(tail(&run(&mut app, 30, dt), 0.2))
    };
    // Identity faces −Z: away from the listener at the origin.
    let away = level(Quat::IDENTITY);
    let toward = level(Quat::from_rotation_y(std::f32::consts::PI));
    assert!(toward > 7.0 * away, "toward {toward} away {away}");
}

// ------------------------------------------------------------------------------- split-screen

#[test]
fn each_emitter_is_heard_by_the_nearest_listener() {
    let dt = 1.0 / 60.0;
    let mut app = app(dt);
    add_sound(&mut app, "hum", tone(300.0, 0.5, 0.3));
    listener_at(&mut app, Vec3::ZERO, Quat::IDENTITY, false);
    listener_at(&mut app, Vec3::X * 100.0, Quat::IDENTITY, false);
    // 97 m from listener 0 (beyond max distance: silent there), 3 m left of listener 1.
    app.world_mut().spawn((
        AudioEmitter {
            min_distance: 2.0,
            max_distance: 40.0,
            ..emitter("hum", 20.0)
        },
        Transform::from_translation(Vec3::X * 97.0),
    ));
    let out = run(&mut app, 30, dt);
    let (l, r) = channels(tail(&out, 0.2));
    assert!(rms(&l) > 0.02, "audible through the near listener");
    assert!(
        rms(&l) > 2.0 * rms(&r),
        "left of listener 1: left {} right {}",
        rms(&l),
        rms(&r)
    );
}

// ------------------------------------------------------------------------------- occlusion

#[test]
fn occlusion_muffles_and_quietens_and_spends_the_ray_budget() {
    let dt = 1.0 / 60.0;
    let mut app = app(dt);
    add_sound(&mut app, "tone", tone(4_000.0, 0.5, 0.3));
    let listener = listener_at(&mut app, Vec3::ZERO, Quat::IDENTITY, false);
    let entity = app
        .world_mut()
        .spawn((
            AudioEmitter {
                occlusion: true,
                ..emitter("tone", 20.0)
            },
            Transform::from_translation(Vec3::NEG_Z * 10.0),
        ))
        .id();
    let clear = run(&mut app, 30, dt);
    {
        let occ = app.world().resource::<AudioOcclusion>();
        let ray = occ
            .pending()
            .iter()
            .find(|r| r.emitter == entity)
            .expect("a ray is queued");
        assert_eq!(ray.listener, Some(listener));
        assert!((ray.to - Vec3::NEG_Z * 10.0).length() < 1e-4);
    }
    // A provider answers "fully occluded" every frame.
    let mut occluded = Vec::new();
    for _ in 0..30 {
        app.world_mut()
            .resource_mut::<AudioOcclusion>()
            .report(entity, 1.0);
        occluded.extend(run(&mut app, 1, dt));
    }
    let open = goertzel(&channels(tail(&clear, 0.2)).0, 4_000.0);
    let shut = goertzel(&channels(tail(&occluded, 0.2)).0, 4_000.0);
    // −15 dB of gain plus a 700 Hz low-pass on a 4 kHz tone: well over 20 dB.
    assert!(shut < open * 0.1, "open {open} occluded {shut}");
    // The change is smoothed: the first occluded frame is still close to the open level.
    let first = goertzel(&channels(&occluded[..1_600]).0, 4_000.0);
    assert!(
        first > open * 0.3,
        "occlusion stepped instead of smoothing: {first} vs {open}"
    );
}

#[test]
fn occlusion_rays_respect_the_per_frame_budget() {
    let dt = 1.0 / 60.0;
    let mut app = app(dt);
    add_sound(&mut app, "hum", tone(300.0, 0.5, 0.05));
    app.world_mut().resource_mut::<AudioOcclusion>().rays_per_frame = 4;
    listener_at(&mut app, Vec3::ZERO, Quat::IDENTITY, false);
    let emitters: Vec<Entity> = (0..10)
        .map(|i| {
            app.world_mut()
                .spawn((
                    AudioEmitter {
                        occlusion: true,
                        ..emitter("hum", 20.0)
                    },
                    Transform::from_translation(Vec3::new(i as f32, 0.0, -5.0)),
                ))
                .id()
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..3 {
        run(&mut app, 1, dt);
        let occ = app.world().resource::<AudioOcclusion>();
        assert!(occ.pending().len() <= 4);
        seen.extend(occ.pending().iter().map(|r| r.emitter));
    }
    assert_eq!(seen.len(), emitters.len(), "round-robin reaches every emitter");
}

// ------------------------------------------------------------------------------- reverb zones

/// Energy of the reverb tail after a short burst, with the listener at `listener`.
fn reverb_tail(listener: Vec3, zone: Option<(AudioReverbZone, Vec3)>) -> f32 {
    let dt = 1.0 / 60.0;
    let mut app = app(dt);
    add_sound(&mut app, "burst", noise(0.08, 0.5));
    listener_at(&mut app, listener, Quat::IDENTITY, false);
    if let Some((zone, centre)) = zone {
        app.world_mut().spawn((zone, Transform::from_translation(centre)));
    }
    // Let the zone settle, then fire the burst next to the listener.
    run(&mut app, 10, dt);
    app.world_mut().spawn((
        AudioEmitter {
            looping: false,
            ..emitter("burst", 20.0)
        },
        Transform::from_translation(listener + Vec3::NEG_Z),
    ));
    let out = run(&mut app, 60, dt);
    // The burst is over after ~0.1 s; measure 0.3–1.0 s.
    rms(&out[(0.3 * f64::from(RATE)) as usize * 2..])
}

#[test]
fn reverb_zones_reverberate_only_around_the_listener() {
    let hall = AudioReverbZone::hall(Vec3::splat(10.0));
    let inside = reverb_tail(Vec3::ZERO, Some((hall.clone(), Vec3::ZERO)));
    let outside = reverb_tail(Vec3::ZERO, Some((hall, Vec3::X * 100.0)));
    let none = reverb_tail(Vec3::ZERO, None);
    assert!(inside > 0.005, "a hall reverberates: {inside}");
    assert!(
        outside < inside * 0.05,
        "far from the zone it is dry: {outside} vs {inside}"
    );
    assert!(none < inside * 0.05, "no zone, no reverb: {none} vs {inside}");
}

#[test]
fn walking_out_of_a_zone_fades_the_reverb_send() {
    let dt = 1.0 / 60.0;
    let mut app = app(dt);
    let listener = listener_at(&mut app, Vec3::ZERO, Quat::IDENTITY, false);
    app.world_mut().spawn((
        AudioReverbZone {
            fade_distance: 4.0,
            ..AudioReverbZone::small_room(Vec3::splat(3.0))
        },
        Transform::default(),
    ));
    let weight_at = |app: &mut App, x: f32| {
        app.world_mut()
            .get_mut::<Transform>(listener)
            .expect("transform")
            .translation = Vec3::X * x;
        run(app, 1, dt);
        app.world().resource::<Audio>().reverb_mix().map(|m| m.weight)
    };
    assert_eq!(weight_at(&mut app, 0.0), Some(1.0));
    let half = weight_at(&mut app, 5.0).expect("mix");
    assert!((half - 0.5).abs() < 1e-4, "{half}");
    assert_eq!(weight_at(&mut app, 20.0), Some(0.0));
}

// ------------------------------------------------------------------------------- HRTF

/// Lag (samples) at which `right` best matches `left`: positive when the left ear hears the
/// signal later.
fn interaural_lag(left: &[f32], right: &[f32], max_lag: i64) -> i64 {
    let mut best = (0i64, f32::MIN);
    for lag in -max_lag..=max_lag {
        let mut sum = 0.0;
        for i in max_lag as usize..left.len() - max_lag as usize {
            sum += right[i] * left[(i as i64 + lag) as usize];
        }
        if sum > best.1 {
            best = (lag, sum);
        }
    }
    best.0
}

#[test]
fn a_binaural_listener_hears_interaural_time_and_level_differences() {
    let dt = 1.0 / 60.0;
    let mut app = app(dt);
    add_sound(&mut app, "noise", noise(1.0, 0.3));
    listener_at(&mut app, Vec3::ZERO, Quat::IDENTITY, true);
    app.world_mut()
        .spawn((emitter("noise", 20.0), Transform::from_translation(Vec3::X * 3.0)));
    let out = run(&mut app, 30, dt);
    let (l, r) = channels(tail(&out, 0.25));
    // The far ear hears it ~0.66 ms (≈ 31 samples) later and quieter (head shadow).
    let lag = interaural_lag(&l, &r, 60);
    assert!((22..=40).contains(&lag), "interaural lag {lag} samples");
    assert!(rms(&r) > 1.3 * rms(&l), "right {} left {}", rms(&r), rms(&l));
    let status = app.world().resource::<Audio>().emitter_status();
    assert!(status[0].binaural);
}

#[test]
fn binaural_rendering_is_opt_in_and_switches_live() {
    let dt = 1.0 / 60.0;
    let mut app = app(dt);
    add_sound(&mut app, "noise", noise(1.0, 0.3));
    let listener = listener_at(&mut app, Vec3::ZERO, Quat::IDENTITY, false);
    // 45° to the right: panned, but both channels carry signal to correlate.
    app.world_mut().spawn((
        emitter("noise", 20.0),
        Transform::from_translation(Vec3::new(3.0, 0.0, -3.0)),
    ));
    let speakers = run(&mut app, 20, dt);
    let (l, r) = channels(tail(&speakers, 0.2));
    assert_eq!(interaural_lag(&l, &r, 60), 0, "speakers: panning only, no delay");
    assert!(!app.world().resource::<Audio>().emitter_status()[0].binaural);

    // The player switches to headphones mid-sound: the voice moves onto a binaural track and
    // keeps playing.
    app.world_mut()
        .get_mut::<AudioListener>(listener)
        .expect("listener")
        .hrtf = true;
    let headphones = run(&mut app, 20, dt);
    assert_eq!(app.world().resource::<Audio>().real_voice_count(), 1);
    assert!(app.world().resource::<Audio>().emitter_status()[0].binaural);
    let (l, r) = channels(tail(&headphones, 0.2));
    assert!(rms(&l) > 0.01 && rms(&r) > 0.01, "still playing");
    // Woodworth delay at 45°: (a/c)(π/4 + sin π/4) ≈ 0.38 ms ≈ 18 samples at 48 kHz.
    let lag = interaural_lag(&l, &r, 60);
    assert!((13..=24).contains(&lag), "45° source: lag {lag}");
}
