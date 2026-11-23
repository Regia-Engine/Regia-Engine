// SPDX-License-Identifier: MPL-2.0
//! Persistent host loops retain ownership across an output teardown (ADR-8052).

use std::time::Duration;

use regia_audio::{Audio, AudioSettings, MixdownSettings, PlaybackParams, SoundAsset, SoundClip};

fn audio() -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    })
}

fn tone() -> SoundAsset {
    let samples: Vec<f32> = (0..48_000)
        .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.2)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, 48_000).expect("clip"))
}

fn render(audio: &mut Audio) -> f32 {
    let mut peak = 0.0_f32;
    for _ in 0..12 {
        audio.tick(1.0 / 60.0);
        for sample in audio.render_offline(800).expect("offline") {
            peak = peak.max(sample.abs());
        }
    }
    // Kira publishes positions at the start of a block. Flush that publication without
    // advancing the sample clock, so cursor assertions measure all rendered frames.
    audio.render_offline(0).expect("publish cursor");
    peak
}

#[test]
fn persistent_loop_keeps_its_id_and_cursor_across_twenty_cycles() {
    let mut audio = audio();
    let id = audio
        .start_persistent_loop(&tone(), PlaybackParams::default())
        .expect("loop");
    assert!(render(&mut audio) > 0.05);
    for _ in 0..20 {
        let position = audio.voice_position(id).expect("playing");
        audio.suspend_output();
        audio.suspend_output();
        assert!(audio.voice_alive(id));
        assert_eq!(audio.real_voice_count(), 0);
        // The offline mixer can finish the standard 10 ms declick tail after teardown.
        render(&mut audio);
        assert!(
            render(&mut audio) < 1e-4,
            "the suspended loop cannot revive on old tracks"
        );
        assert!(audio.resume_output());
        assert!(audio.resume_output());
        // Give the renderer a block to apply the new handle's seek/fade.
        audio.render_offline(800).expect("offline");
        audio.render_offline(0).expect("publish cursor");
        let resumed = audio.voice_position(id).expect("same id");
        assert!(
            (resumed - (position + 1.0 / 60.0).rem_euclid(1.0)).abs() < 0.002,
            "{position} -> {resumed}"
        );
        assert!(render(&mut audio) > 0.05);
        assert_eq!(audio.real_voice_count(), 1, "no duplicate on double resume");
        assert_eq!(audio.virtual_voice_count(), 0);
    }
    assert_eq!(audio.suspend_count(), 20);
    assert_eq!(audio.resume_count(), 20);
    audio.stop_voice(id, Duration::ZERO);
    render(&mut audio);
    assert!(!audio.voice_alive(id));
    assert!(render(&mut audio) < 1e-4);
}

#[test]
fn stop_while_suspended_cancels_recovery() {
    let mut audio = audio();
    let id = audio
        .start_persistent_loop(&tone(), PlaybackParams::default())
        .expect("loop");
    render(&mut audio);
    audio.suspend_output();
    audio.stop_voice(id, Duration::ZERO);
    assert!(audio.resume_output());
    assert!(!audio.voice_alive(id));
    assert!(render(&mut audio) < 1e-4);
}

#[test]
fn persistent_loop_created_while_suspended_waits_for_output() {
    let mut audio = audio();
    audio.suspend_output();
    let id = audio
        .start_persistent_loop(&tone(), PlaybackParams::default())
        .expect("queued loop");
    assert!(audio.voice_alive(id));
    assert_eq!(audio.real_voice_count(), 0);
    assert!(render(&mut audio) < 1e-4);
    assert!(audio.resume_output());
    assert!(render(&mut audio) > 0.05);
    assert!(audio.voice_playing(id));
}

#[test]
fn pause_and_live_controls_survive_output_recovery() {
    let mut audio = audio();
    let id = audio
        .start_persistent_loop(&tone().with_default_pitch(0.5), PlaybackParams::default())
        .expect("loop");
    audio.set_voice_volume_db(id, -18.0, 0.0);
    audio.set_voice_pitch(id, 2.0, 0.0);
    audio.set_voice_pan(id, 1.0, 0.0);
    render(&mut audio);
    audio.pause_voice(id, Duration::ZERO);
    render(&mut audio);
    audio.suspend_output();
    audio.seek_voice(id, 0.1);
    assert!(audio.resume_output());
    assert!(render(&mut audio) < 1e-4, "recovery must not unpause");
    audio.resume_voice(id, Duration::ZERO);
    let peak = render(&mut audio);
    assert!((0.01..0.04).contains(&peak), "retained -18 dB: {peak}");
    let position = audio.voice_position(id).expect("position");
    assert!(
        (position - 0.5).abs() < 0.01,
        "retained 2x rate and seek: {position}"
    );
    let samples = audio.render_offline(800).expect("offline");
    assert!(
        samples.iter().step_by(2).all(|s| s.abs() < 1e-4),
        "retained right pan"
    );
}

#[test]
fn mixdown_does_not_resurrect_persistent_preview_loops() {
    let mut audio = audio();
    let id = audio
        .start_persistent_loop(&tone(), PlaybackParams::default())
        .expect("loop");
    render(&mut audio);
    audio.begin_mixdown(&MixdownSettings::default()).expect("mixdown");
    assert!(!audio.voice_alive(id));
    assert!(audio.end_mixdown());
    audio.suspend_output();
    assert!(audio.resume_output());
    assert!(!audio.voice_alive(id));
    assert!(render(&mut audio) < 1e-4);
}

#[test]
fn recovery_obeys_the_voice_budget_and_keeps_virtual_loop_ownership() {
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        voice_limit: Some(1),
        ..AudioSettings::default()
    });
    let first = audio
        .start_persistent_loop(&tone(), PlaybackParams::default())
        .expect("first");
    let second = audio
        .start_persistent_loop(&tone(), PlaybackParams::default())
        .expect("second");
    assert_eq!(audio.real_voice_count(), 1);
    assert_eq!(audio.virtual_voice_count(), 1);
    audio.suspend_output();
    assert_eq!(audio.persistent_loop_count(), 2);
    assert!(audio.resume_output());
    assert_eq!(
        audio.real_voice_count(),
        1,
        "recovery cannot briefly exceed the budget"
    );
    assert_eq!(audio.virtual_voice_count(), 1);
    assert!(audio.voice_alive(first) && audio.voice_alive(second));
    assert!(render(&mut audio) > 0.05);
    audio.stop_voice(first, Duration::ZERO);
    audio.stop_voice(second, Duration::ZERO);
    assert_eq!(audio.persistent_loop_count(), 0);
}

#[test]
fn edits_while_suspended_and_asset_loop_defaults_are_retained() {
    use regia_audio::LoopRegion;
    let mut audio = audio();
    let asset = tone().with_default_loop_region(LoopRegion {
        start_secs: 0.2,
        end_secs: Some(0.4),
    });
    let id = audio
        .start_persistent_loop(&asset, PlaybackParams::default())
        .expect("loop");
    render(&mut audio);
    audio.suspend_output();
    audio.set_voice_volume_db(id, -12.0, 0.0);
    audio.set_voice_pitch(id, 1.0, 0.0);
    audio.set_voice_pan(id, -1.0, 0.0);
    audio.seek_voice(id, 0.25);
    assert!(audio.resume_output());
    let peak = render(&mut audio);
    // Hard pan has sqrt(2) gain in the surviving channel (equal-power pan law).
    let expected = 0.2 * std::f32::consts::SQRT_2 * 10.0_f32.powf(-12.0 / 20.0);
    assert!((peak - expected).abs() < 0.0001, "suspended level change: {peak}");
    let position = audio.voice_position(id).expect("position");
    assert!(
        (position - 0.25).abs() < 0.01,
        "the authored 0.2 s loop wrapped: {position}"
    );
    let samples = audio.render_offline(800).expect("offline");
    assert!(samples.iter().skip(1).step_by(2).all(|s| s.abs() < 1e-4));
}

#[test]
fn persistent_loop_rejects_unbounded_sources_and_unknown_buses() {
    use regia_audio::AudioError;
    for suspended in [false, true] {
        let mut audio = audio();
        if suspended {
            audio.suspend_output();
        }
        for duration in [f64::NAN, f64::INFINITY] {
            let asset = SoundAsset::from_streaming_factory(
                || panic!("a rejected loop must not open a decoder"),
                duration,
                "unbounded",
            );
            assert!(matches!(
                audio.start_persistent_loop(&asset, PlaybackParams::default()),
                Err(AudioError::Invalid(_))
            ));
        }
        assert!(matches!(
            audio.start_persistent_loop(&tone(), PlaybackParams::on_bus("missing_bus")),
            Err(AudioError::UnknownBus(_))
        ));
        assert_eq!(audio.persistent_loop_count(), 0);
        assert_eq!(audio.real_voice_count(), 0);
        assert_eq!(audio.virtual_voice_count(), 0);
    }
}

#[test]
fn invalid_controls_cannot_poison_a_recovered_loop() {
    let mut audio = audio();
    let id = audio
        .start_persistent_loop(&tone(), PlaybackParams::default())
        .expect("loop");
    audio.suspend_output();
    audio.set_voice_volume_db(id, f32::NAN, 0.0);
    audio.set_voice_pitch(id, f32::NAN, 0.0);
    audio.set_voice_pan(id, f32::NAN, 0.0);
    audio.seek_voice(id, f64::INFINITY);
    assert!(audio.resume_output());
    assert!(render(&mut audio) < 1e-4, "invalid volume stays silent");
    audio.set_voice_volume_db(id, 0.0, 0.0);
    assert!(
        render(&mut audio) > 0.05,
        "the retained original-speed voice remains usable"
    );
    let samples = audio.render_offline(800).expect("offline");
    assert!(samples.iter().all(|sample| sample.is_finite()));
}
