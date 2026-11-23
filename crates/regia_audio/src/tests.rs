// SPDX-License-Identifier: MPL-2.0
//! Tests for the audio crate (one-shot slice + facade). Every test runs against the offline
//! backend; no audio output device is opened.

use regia_app::{App, Update};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;

use crate::audio::Audio;
use crate::clip::{SoundClip, SoundClipError};
use crate::error::AudioError;
use crate::plugin::{AudioPlugin, drain_one_shots};
use crate::settings::AudioSettings;

use std::time::Duration;

/// A short test clip: 16 samples at 48 kHz.
fn test_clip() -> SoundClip {
    SoundClip::new(vec![0.25; 16], 48_000).expect("16 samples at 48 kHz are a valid clip")
}

/// A headless app with the audio plugin.
fn headless_app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    app.add_plugins(AudioPlugin);
    app
}

#[test]
fn play_increments_the_count_and_records_the_rate() {
    fn play_once(mut audio: ResMut<Audio>) {
        audio.play(&test_clip()).expect("one-shot enqueue");
    }

    let mut app = headless_app();
    app.add_systems(Update, play_once.before(drain_one_shots));
    app.finish_plugins();
    app.update();

    let audio = app.world().resource::<Audio>();
    assert_eq!(audio.played_count(), 1);
    let played = audio.played();
    assert_eq!(played.len(), 1);
    assert_eq!(played[0].clip_len, 16);
    assert_eq!(played[0].sample_rate, 48_000);
    assert_eq!(played[0].position, None);
}

#[test]
fn play_at_records_the_position() {
    const POSITION: Vec3 = Vec3::new(2.0, 1.0, -3.0);

    fn play_at_position(mut audio: ResMut<Audio>) {
        audio.play_at(&test_clip(), POSITION).expect("one-shot enqueue");
    }

    let mut app = headless_app();
    app.add_systems(Update, play_at_position.before(drain_one_shots));
    app.finish_plugins();
    app.update();

    let audio = app.world().resource::<Audio>();
    assert_eq!(audio.played_count(), 1);
    let played = audio.played();
    assert_eq!(played.len(), 1);
    assert_eq!(played[0].position, Some(POSITION));
}

#[test]
fn two_plays_in_one_frame_both_count() {
    fn play_twice(mut audio: ResMut<Audio>) {
        let clip = test_clip();
        audio.play(&clip).expect("first one-shot enqueue");
        audio.play(&clip).expect("second one-shot enqueue");
    }

    let mut app = headless_app();
    app.add_systems(Update, play_twice.before(drain_one_shots));
    app.finish_plugins();
    app.update();

    let audio = app.world().resource::<Audio>();
    assert_eq!(audio.played_count(), 2);
    assert_eq!(audio.played().len(), 2);
}

#[test]
fn zero_length_clip_is_rejected_with_an_error_not_a_panic() {
    // Rejected where clips are built:
    assert_eq!(
        SoundClip::new(Vec::<f32>::new(), 48_000),
        Err(SoundClipError::Empty)
    );
    assert_eq!(SoundClip::new(vec![0.0], 0), Err(SoundClipError::ZeroSampleRate));

    // Defensively also rejected at the play boundary:
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    let empty = SoundClip {
        samples: [].into(),
        sample_rate: 48_000,
    };
    assert_eq!(audio.play(&empty), Err(AudioError::EmptyClip));
    assert_eq!(audio.play_at(&empty, Vec3::ZERO), Err(AudioError::EmptyClip));
    assert_eq!(audio.played_count(), 0);
    assert!(audio.played().is_empty());
}

#[test]
fn headless_setting_uses_the_offline_backend() {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    assert!(audio.is_headless());

    let clip = test_clip();
    audio.play(&clip).expect("enqueue");
    audio.play_at(&clip, Vec3::ONE).expect("enqueue");
    audio.drain();

    assert_eq!(audio.played_count(), 2);
    let played = audio.played();
    assert_eq!(played.len(), 2);
    assert_eq!(played[0].position, None);
    assert_eq!(played[1].position, Some(Vec3::ONE));
}

#[test]
fn headless_backend_renders_audible_frames() {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    // A loud 1 kHz tone played 2D.
    let clip = SoundClip::new(
        (0..4_800)
            .map(|i| ((i as f32 / 48_000.0) * 1_000.0 * std::f32::consts::TAU).sin() * 0.5)
            .collect::<Vec<_>>(),
        48_000,
    )
    .expect("tone");
    audio.play(&clip).expect("enqueue");
    audio.drain();
    let frames = audio.render_offline(4_800).expect("offline render");
    let peak = frames.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
    assert!(peak > 0.1, "the tone must be audible, peak {peak}");
}

#[test]
fn default_settings_have_the_spec_bus_tree_and_duck_rules() {
    let settings = AudioSettings::default();
    let names: Vec<String> = settings.buses.buses.iter().map(|b| b.name.clone()).collect();
    for expected in ["Master", "Music", "Sfx", "World", "Ui", "Voice", "Ambience"] {
        assert!(names.iter().any(|n| n == expected), "missing {expected}");
    }
    assert!(
        settings
            .duck_rules
            .iter()
            .any(|r| r.when == "Voice" && r.target == "Music")
    );
    assert_eq!(settings.voice_limit, None);
    assert_eq!(crate::settings::default_voice_limit(), 64);
}

// --------------------------------------------------------------------- the per-voice handle API

#[test]
fn virtualising_a_voice_keeps_its_playback_position() {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    let samples = vec![0.1_f32; 48_000];
    let asset = crate::sound::SoundAsset::from_clip(&SoundClip::new(samples, 48_000).expect("clip"));
    audio.register_cue(crate::cue::SoundCue::new(
        "long",
        vec![crate::cue::CueEntry { asset, weight: 1.0 }],
    ));
    let id = audio.play_cue("long").expect("registered").expect("played");
    for _ in 0..30 {
        audio.tick(1.0 / 60.0);
        let _ = audio.render_offline(800);
    }
    let entry = audio.voices.get_mut(id).expect("voice");
    entry.virtualise();
    assert!(entry.instance.is_none());
    assert!(
        entry.virtual_cursor_secs > 0.1,
        "cursor kept the position, got {}",
        entry.virtual_cursor_secs
    );
}

#[test]
fn play_cue_with_no_entries_plays_nothing() {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    audio.register_cue(crate::cue::SoundCue::new("empty", Vec::new()));
    assert_eq!(audio.play_cue("empty").expect("registered"), None);
    assert_eq!(audio.play_cue_seeded("empty", 7).expect("registered"), None);
    assert_eq!(audio.real_voice_count(), 0);
}

#[test]
fn voice_handle_api_pause_resume_seek_state() {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    // A looping tone so the voice stays alive through the whole test.
    let samples: Vec<f32> = (0..4_800)
        .map(|i| ((i as f32 / 48_000.0) * 440.0 * std::f32::consts::TAU).sin() * 0.4)
        .collect();
    let asset = crate::sound::SoundAsset::from_clip(&SoundClip::new(samples, 48_000).expect("tone"));
    let mut cue = crate::cue::SoundCue::new("loop", vec![crate::cue::CueEntry { asset, weight: 1.0 }]);
    cue.max_concurrent = None;
    audio.register_cue(cue);
    let id = audio.play_cue("loop").expect("registered").expect("played");
    assert_eq!(audio.real_voice_count(), 1);
    assert_eq!(audio.voice_state(id), Some(kira::sound::PlaybackState::Playing));

    audio.pause_voice(id, Duration::from_millis(5));
    audio.tick(1.0 / 60.0);
    let _ = audio.render_offline(800);
    audio.tick(1.0 / 60.0);
    let _ = audio.render_offline(800);
    assert_eq!(audio.voice_state(id), Some(kira::sound::PlaybackState::Paused));

    audio.resume_voice(id, Duration::from_millis(5));
    audio.tick(1.0 / 60.0);
    let _ = audio.render_offline(800);
    assert_eq!(audio.voice_state(id), Some(kira::sound::PlaybackState::Playing));

    // Seeking moves the position (the seek is a command, applied at the next render).
    let before = audio.voice_position(id);
    audio.seek_voice(id, 0.05);
    audio.tick(1.0 / 60.0);
    let _ = audio.render_offline(800);
    let after = audio.voice_position(id);
    assert!(
        before.is_some()
            && after.is_some()
            && (after.expect("checked") - before.expect("checked")).abs() > 0.01,
        "seek moved the cursor: {before:?} → {after:?}"
    );

    // Live tweens on volume/pan/pitch go through (no crash; state observable).
    audio.set_voice_volume_db(id, -6.0, 0.02);
    audio.set_voice_pitch(id, 0.5, 0.02);
    audio.set_voice_pan(id, -0.5, 0.02);
    audio.tick(1.0 / 60.0);
    let _ = audio.render_offline(800);
    assert_eq!(audio.voice_state(id), Some(kira::sound::PlaybackState::Playing));

    assert!(audio.voice_playing(id), "a playing voice keeps its caption");
    // A dead id reports nothing and never panics.
    let dead = crate::voices::SoundInstanceId(999);
    assert_eq!(audio.voice_state(dead), None);
    assert!(!audio.voice_playing(dead), "a missing id is not playing");
    audio.stop_voice(id, Duration::ZERO);
    assert!(
        !audio.voice_playing(id),
        "stop takes the instance, so the voice is no longer playing"
    );
    assert_eq!(audio.voice_position(dead), None);
    audio.pause_voice(dead, Duration::ZERO);
    audio.resume_voice(dead, Duration::ZERO);
    audio.set_voice_volume_db(dead, 0.0, 0.0);
    audio.seek_voice(dead, 0.1);
}

#[test]
fn user_settings_apply_volumes_and_mutes() {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    audio.apply_user_settings(
        &crate::settings::AudioUserSettings {
            voice_chat: Default::default(),
            bus_volumes: [("Master".to_string(), -6.0), ("Music".to_string(), -12.0)]
                .into_iter()
                .collect(),
            muted: vec!["Ambience".to_string()],
        },
        Duration::ZERO,
    );
    assert_eq!(audio.bus_volume("Master").expect("bus"), -6.0);
    assert_eq!(audio.bus_volume("Music").expect("bus"), -12.0);
    assert_eq!(audio.bus_muted("Ambience"), Some(true));
    assert_eq!(audio.bus_muted("Voice"), Some(false));
    // Unknown buses are skipped, not errors (the settings screen may outlive a layout change).
    audio.apply_user_settings(
        &crate::settings::AudioUserSettings {
            voice_chat: Default::default(),
            bus_volumes: [("Nope".to_string(), -6.0)].into_iter().collect(),
            muted: vec!["Nope".to_string()],
        },
        Duration::ZERO,
    );
    assert_eq!(audio.bus_muted("Nope"), None);
}

/// A non-finite bus volume is refused and leaves the bus as it was (gap-audit B, stability-smalls):
/// NaN, +inf and -inf through the direct call and through user settings.
#[test]
fn non_finite_bus_volume_is_refused() {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    audio
        .set_bus_volume("Music", -6.0, Duration::ZERO)
        .expect("a finite volume");
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(
            matches!(
                audio.set_bus_volume("Music", bad, Duration::from_millis(100)),
                Err(AudioError::Invalid(_))
            ),
            "{bad} refused"
        );
        assert_eq!(audio.bus_volume("Music").expect("bus"), -6.0, "{bad} left it");
    }
    let mut settings = crate::settings::AudioUserSettings::default();
    settings.bus_volumes.insert("Music".into(), f32::NAN);
    audio.apply_user_settings(&settings, Duration::ZERO);
    assert_eq!(
        audio.bus_volume("Music").expect("bus"),
        -6.0,
        "settings NaN skipped"
    );
    audio.tick(1.0);
    assert_eq!(
        audio.bus_volume("Music").expect("bus"),
        -6.0,
        "still -6 dB after a tick"
    );
}

#[test]
fn unknown_bus_is_an_error_and_user_settings_skip_it() {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    assert!(matches!(
        audio.set_bus_volume("Nope", -12.0, Duration::ZERO),
        Err(AudioError::UnknownBus(name)) if name == "Nope"
    ));
    assert!(matches!(audio.bus_volume("Nope"), Err(AudioError::UnknownBus(_))));
    assert!(matches!(
        audio.set_bus_mute("Nope", true),
        Err(AudioError::UnknownBus(_))
    ));
    let music = audio.bus_volume("Music").expect("bus");
    // A misspelled bus leaves the real one untouched; a mixed list changes only real buses.
    audio.apply_user_settings(
        &crate::settings::AudioUserSettings {
            voice_chat: Default::default(),
            bus_volumes: [("Musci".to_string(), -30.0)].into_iter().collect(),
            muted: Vec::new(),
        },
        Duration::ZERO,
    );
    assert_eq!(audio.bus_volume("Music").expect("bus"), music);
    audio.apply_user_settings(
        &crate::settings::AudioUserSettings {
            voice_chat: Default::default(),
            bus_volumes: [("Musci".to_string(), -30.0), ("Music".to_string(), -6.0)]
                .into_iter()
                .collect(),
            muted: Vec::new(),
        },
        Duration::ZERO,
    );
    assert_eq!(audio.bus_volume("Music").expect("bus"), -6.0);
}

#[test]
fn output_error_drain_is_empty_offline_and_never_panics() {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    assert!(
        audio.take_output_errors().is_empty(),
        "the offline backend has no cpal stream errors to report"
    );
    // Drain again after suspend/resume cycles (the device backend keeps its error queue across
    // stream restarts; the offline one stays empty).
    audio.suspend_output();
    audio.resume_output();
    assert!(audio.take_output_errors().is_empty());
}

#[test]
fn rejected_cue_play_does_not_start_the_cooldown() {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    let mut cue = crate::cue::SoundCue::new(
        "hit",
        vec![crate::cue::CueEntry {
            asset: crate::test_support::asset(),
            weight: 1.0,
        }],
    );
    cue.cooldown = Duration::from_secs(10);
    cue.bus = Some("NoSuchBus".into());
    audio.register_cue(cue);
    assert!(audio.play_cue("hit").is_err(), "unknown bus rejects the play");
    // A rejected play must not leave the cue on cooldown: the retry reaches the backend again
    // (and fails again) instead of being silently swallowed as `Ok(None)`.
    assert!(audio.play_cue("hit").is_err());
}

#[test]
fn preempting_with_an_empty_bank_releases_the_announcer() {
    use crate::announcer::{AnnounceOutcome, AnnouncePriority, AnnouncerBank};
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    audio.register_announcer_bank(
        "line",
        AnnouncerBank::new(vec![crate::test_support::asset()], AnnouncePriority::Normal),
    );
    audio.register_announcer_bank("empty", AnnouncerBank::new(vec![], AnnouncePriority::Critical));
    assert_eq!(audio.announce("line"), AnnounceOutcome::Started);
    assert_eq!(audio.announce("empty"), AnnounceOutcome::Preempted);
    // The pre-empted line is stopped and nothing replaced it: the announcer is idle again, so an
    // equal-priority line starts instead of being rejected behind a voice that no longer exists.
    assert_eq!(audio.announce("line"), AnnounceOutcome::Started);
}

#[test]
fn layout_muted_bus_stays_muted_through_ticks_and_resume() {
    use crate::bus::{BusDef, BusLayout};
    let bus = |name: &str, parent: Option<&str>, mute: bool| BusDef {
        name: name.into(),
        parent: parent.map(str::to_string),
        volume_db: 0.0,
        mute,
        effects: vec![],
        sends: vec![],
        bypass_medium: false,
    };
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        buses: BusLayout {
            buses: vec![bus("Master", None, false), bus("Quiet", Some("Master"), true)],
            send_buses: vec![],
            ..BusLayout::default()
        },
        ..AudioSettings::default()
    });
    assert_eq!(audio.bus_muted("Quiet"), Some(true));
    assert_eq!(audio.bus_muted("Master"), Some(false));
    audio.tick(1.0 / 60.0);
    assert_eq!(audio.bus_muted("Quiet"), Some(true));
    audio.suspend_output();
    audio.resume_output();
    audio.tick(1.0 / 60.0);
    assert_eq!(audio.bus_muted("Quiet"), Some(true));
}

#[test]
fn slider_move_under_a_snapshot_keeps_the_snapshot_level() {
    use crate::snapshot::AudioSnapshot;
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    audio.register_snapshot(AudioSnapshot {
        name: "Paused".into(),
        bus_volumes: [("Music".to_string(), -40.0)].into_iter().collect(),
        ..AudioSnapshot::default()
    });
    audio.push_snapshot("Paused", Duration::ZERO).expect("push");
    audio
        .set_bus_volume("Music", 0.0, Duration::ZERO)
        .expect("slider");
    assert_eq!(
        audio.bus_volume("Music").expect("bus"),
        -40.0,
        "snapshot level holds"
    );
    audio.pop_snapshot(Duration::ZERO).expect("pop");
    assert_eq!(
        audio.bus_volume("Music").expect("bus"),
        0.0,
        "pop returns to the new slider base"
    );
}

#[test]
fn a_voice_ends_on_the_sample_clock() {
    let settings = AudioSettings {
        headless: true,
        sample_rate: 48_000,
        ..AudioSettings::default()
    };
    let mut audio = Audio::new(&settings);
    assert_eq!(audio.sample_clock(), None);
    audio.render_offline(480).expect("offline");
    let started = audio.sample_clock().expect("rendered").frames;
    assert_eq!(started, 480);
    let clip = SoundClip::new(vec![0.25; 2_400], 48_000).expect("a valid clip");
    let id = audio
        .start_asset(&crate::sound::SoundAsset::from_clip(&clip), Default::default())
        .expect("starts");
    // Rendered in mixer-sized blocks: the voice stops within a block of its last frame.
    let block = 128;
    while audio
        .voice_state(id)
        .is_some_and(|s| s != kira::sound::PlaybackState::Stopped)
    {
        audio.render_offline(block).expect("offline");
        assert!(
            audio.sample_clock().expect("rendered").frames < started + 4_800,
            "it never ended"
        );
    }
    let ended = audio.sample_clock().expect("rendered").frames;
    assert!(
        (2_400..=2_400 + 2 * block as u64).contains(&(ended - started)),
        "ended {} frames in",
        ended - started
    );
}

#[test]
fn a_tiny_offline_sample_rate_is_floored_and_renders() {
    for sample_rate in [0, 100, 195, 800] {
        let mut audio = Audio::new(&AudioSettings {
            headless: true,
            sample_rate,
            ..AudioSettings::default()
        });
        let out = audio.render_offline(256).expect("render");
        assert!(out.iter().all(|s| s.is_finite()), "rate {sample_rate}");
    }
}
