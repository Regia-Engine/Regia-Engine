// SPDX-License-Identifier: MPL-2.0
//! v0.1.6 regression tests for the output suspend/resume path and patch filters
//! (`docs/specs/16-audio.md` § 3a, § 4; DO27 reports AUDIO_ZOMBIE and AUDIO_PATCH_FILTER).
//!
//! Every assertion is on samples rendered by the offline backend or on public mixer state.

use std::collections::HashMap;
use std::time::Duration;

use regia_app::{App, Update};
use regia_audio::{
    Audio, AudioError, AudioPatch, AudioPatchDef, AudioPlugin, AudioSettings, LoopRegion, MixdownSettings,
    PatchCurveDef, PatchLayerDef, PatchParamDef, PlaybackParams, SoundAsset, SoundClip, drain_one_shots,
};
use regia_ecs::prelude::*;
use regia_platform::SurfacesHidden;

const RATE: u32 = 48_000;
const FRAME: f64 = 1.0 / 60.0;

fn headless() -> Audio {
    Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    })
}

/// Half a second of `hz` (whole cycles at 48 kHz for the test frequencies), amplitude `amp`.
fn tone(hz: f32, amp: f32) -> SoundAsset {
    let len = RATE as usize / 2;
    let samples: Vec<f32> = (0..len)
        .map(|i| ((i as f32 / RATE as f32) * hz * std::f32::consts::TAU).sin() * amp)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("sine clip"))
}

/// Looping playback on `bus`.
fn looping_on(bus: &str) -> PlaybackParams {
    PlaybackParams {
        bus: Some(bus.into()),
        loop_region: Some(LoopRegion {
            start_secs: 0.0,
            end_secs: None,
        }),
        ..PlaybackParams::default()
    }
}

/// Goertzel magnitude at `hz` over the left channel of interleaved stereo (≈ peak / 2 for a sine).
fn tone_level(frames: &[f32], hz: f32) -> f32 {
    let left: Vec<f32> = frames.iter().step_by(2).copied().collect();
    let k = std::f32::consts::TAU * hz / RATE as f32;
    let coeff = 2.0 * k.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in &left {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt() / left.len().max(1) as f32
}

/// Ticks and renders `seconds` of audio (static sounds only: no real-time pacing needed).
fn run(audio: &mut Audio, seconds: f64) -> Vec<f32> {
    let mut out = Vec::new();
    for _ in 0..(seconds / FRAME).round() as usize {
        audio.tick(FRAME);
        out.extend_from_slice(
            &audio
                .render_offline((f64::from(RATE) * FRAME) as usize)
                .expect("offline"),
        );
    }
    out
}

fn voices(audio: &Audio) -> usize {
    audio.real_voice_count() + audio.virtual_voice_count()
}

fn db(ratio: f32) -> f32 {
    20.0 * ratio.max(1e-9).log10()
}

/// The level a 220 Hz loop on `bus` reaches on a fresh mixer with `setup` applied.
fn reference_level(bus: &str, hz: f32, setup: impl FnOnce(&mut Audio)) -> f32 {
    let mut audio = headless();
    setup(&mut audio);
    audio
        .play_asset(&tone(hz, 0.3), looping_on(bus))
        .expect("enqueue");
    audio.drain();
    let _ = run(&mut audio, 0.3);
    tone_level(&run(&mut audio, 0.2), hz)
}

/// AUDIO_ZOMBIE: a looping Voice-bus sound playing at the suspend instant died with the backend.
/// It must stop counting as Voice activity (the −6 dB Music duck releases within its 1 s release)
/// and must give its voice-budget slot back.
#[test]
fn voice_duck_releases_and_the_budget_recovers_after_suspend_resume() {
    let unducked = reference_level("Music", 220.0, |_| {});

    let mut audio = headless();
    audio
        .play_asset(&tone(220.0, 0.3), looping_on("Music"))
        .expect("enqueue");
    audio.drain();
    let baseline = voices(&audio);
    audio
        .play_asset(&tone(660.0, 0.3), looping_on("Voice"))
        .expect("enqueue");
    audio.drain();
    assert_eq!(voices(&audio), baseline + 1);
    let ducked = tone_level(&run(&mut audio, 0.5), 220.0);
    assert!(
        db(ducked / unducked) < -3.0,
        "the Voice loop ducks the Music bed before the suspend ({ducked} vs {unducked})"
    );

    audio.suspend_output();
    assert!(audio.resume_output(), "the offline backend resumes");
    // The host restarts its bed (playing sounds are not restored across a suspend, spec § 4).
    audio
        .play_asset(&tone(220.0, 0.3), looping_on("Music"))
        .expect("enqueue");
    audio.drain();

    // Release is 1.0 s: after 1.2 s the duck is gone.
    let _ = run(&mut audio, 1.2);
    let after = run(&mut audio, 0.2);
    let bed = tone_level(&after, 220.0);
    assert!(
        db(bed / unducked).abs() < 1.0,
        "the duck released after the resume: bed {bed} vs unducked {unducked} ({:.1} dB)",
        db(bed / unducked)
    );
    assert!(
        tone_level(&after, 660.0) < 1e-3,
        "the Voice loop did not survive the suspend"
    );
    assert_eq!(
        voices(&audio),
        baseline,
        "the voice budget returns to its baseline"
    );
    assert_eq!(audio.real_voice_count(), baseline);
}

/// The voice-chat loopback counts as Voice activity while it runs; a suspend ends it (a host
/// restarts it with `start_voice_loopback`), so it cannot hold the Music duck either.
#[test]
fn voice_loopback_ends_with_the_suspend_and_releases_the_duck() {
    let unducked = reference_level("Music", 220.0, |_| {});
    let mut audio = headless();
    audio.start_voice_loopback(40).expect("loopback starts");
    assert!(audio.voice_stats().is_some_and(|stats| stats.running));
    audio.suspend_output();
    audio.resume_output();
    assert!(
        audio.voice_stats().is_none(),
        "the loopback ended with the suspend"
    );
    audio
        .play_asset(&tone(220.0, 0.3), looping_on("Music"))
        .expect("enqueue");
    audio.drain();
    let _ = run(&mut audio, 1.2);
    let bed = tone_level(&run(&mut audio, 0.2), 220.0);
    assert!(
        db(bed / unducked).abs() < 1.0,
        "no Voice duck after the resume: {bed} vs {unducked}"
    );
}

/// AUDIO_ZOMBIE, repeated: each cycle must not leak a voice slot.
#[test]
fn suspend_resume_cycles_do_not_leak_voice_slots() {
    let mut audio = headless();
    for _ in 0..5 {
        audio
            .play_asset(&tone(660.0, 0.3), looping_on("Voice"))
            .expect("enqueue");
        audio.drain();
        let _ = run(&mut audio, 0.1);
        audio.suspend_output();
        audio.resume_output();
    }
    let _ = run(&mut audio, 0.1);
    assert_eq!(voices(&audio), 0, "no voice of a torn-down backend survives");
}

/// Host-set bus gains and mutes survive a suspend/resume (the rebuilt tracks get the current
/// bus state, not the layout defaults).
#[test]
fn host_bus_gain_and_mute_survive_suspend_resume() {
    let host_mix = |audio: &mut Audio| {
        audio
            .set_bus_volume("Music", -12.0, Duration::ZERO)
            .expect("Music");
        audio.set_bus_mute("Ambience", true).expect("Ambience");
    };
    let expected = reference_level("Music", 220.0, host_mix);
    let layout = reference_level("Music", 220.0, |_| {});
    assert!(
        db(expected / layout) < -6.0,
        "the reference really is quieter ({expected} vs {layout})"
    );

    let mut audio = headless();
    host_mix(&mut audio);
    let _ = run(&mut audio, 0.1);
    audio.suspend_output();
    audio.resume_output();
    audio
        .play_asset(&tone(220.0, 0.3), looping_on("Music"))
        .expect("enqueue");
    audio
        .play_asset(&tone(440.0, 0.3), looping_on("Ambience"))
        .expect("enqueue");
    audio.drain();
    let _ = run(&mut audio, 0.3);
    let out = run(&mut audio, 0.2);
    let music = tone_level(&out, 220.0);
    assert!(
        db(music / expected).abs() < 1.0,
        "Music keeps the host's −12 dB: {music} vs {expected} ({:.1} dB)",
        db(music / expected)
    );
    assert!(tone_level(&out, 440.0) < 1e-3, "Ambience stays muted");
    assert_eq!(audio.bus_volume("Music").ok(), Some(-12.0));
    assert_eq!(audio.bus_muted("Ambience"), Some(true));
}

/// Host-side recovery state for the double-resume test.
#[derive(Resource, Default)]
struct HostRecovery {
    recovered: u32,
}

/// A host that re-opens the stream itself in `Update`, restores its mix and restarts its loop.
fn host_recovers(hidden: Res<SurfacesHidden>, mut host: ResMut<HostRecovery>, mut audio: ResMut<Audio>) {
    if hidden.0 || audio.resume_count() >= audio.suspend_count() {
        return;
    }
    assert!(audio.resume_output(), "the host re-opens the stream");
    audio
        .set_bus_volume("Music", -12.0, Duration::ZERO)
        .expect("Music");
    audio
        .play_asset(&tone(220.0, 0.3), looping_on("Music"))
        .expect("enqueue");
    host.recovered += 1;
}

/// AUDIO_ZOMBIE item 4: a host resume in `Update` plus the output gate in `PostUpdate` rebuild
/// once; the loop the host restarted keeps playing at the host's bus volume.
#[test]
fn host_resume_and_gate_rebuild_once() {
    let expected = reference_level("Music", 220.0, |audio| {
        audio
            .set_bus_volume("Music", -12.0, Duration::ZERO)
            .expect("Music");
    });

    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SurfacesHidden(false));
    app.init_resource::<HostRecovery>();
    app.add_plugins(AudioPlugin);
    app.add_systems(Update, host_recovers.before(drain_one_shots));
    app.update();

    app.world_mut().resource_mut::<SurfacesHidden>().0 = true;
    app.update();
    assert_eq!(
        app.world().resource::<Audio>().suspend_count(),
        1,
        "hiding suspends"
    );

    app.world_mut().resource_mut::<SurfacesHidden>().0 = false;
    app.update();
    assert_eq!(app.world().resource::<HostRecovery>().recovered, 1);
    let audio = app.world().resource::<Audio>();
    assert_eq!(audio.resume_count(), 1, "the gate did not resume a second time");
    assert_eq!(audio.real_voice_count(), 1, "the host's loop is alive");

    app.update();
    let mut audio = app.world_mut().resource_mut::<Audio>();
    let _ = run(&mut audio, 0.3);
    let level = tone_level(&run(&mut audio, 0.2), 220.0);
    assert!(
        db(level / expected).abs() < 1.0,
        "the restarted loop plays at the host's −12 dB: {level} vs {expected}"
    );
}

/// Resuming a live output is a no-op: no bus rebuild, playing sounds keep playing.
#[test]
fn resume_on_a_live_output_is_a_no_op() {
    let mut audio = headless();
    audio
        .play_asset(&tone(220.0, 0.3), looping_on("Music"))
        .expect("enqueue");
    audio.drain();
    let before = tone_level(&run(&mut audio, 0.2), 220.0);
    assert!(audio.resume_output(), "a live output reports resumed");
    assert_eq!(audio.resume_count(), 0, "nothing was re-opened");
    let after = tone_level(&run(&mut audio, 0.2), 220.0);
    assert!(
        db(after / before).abs() < 0.5,
        "the loop kept playing: {after} vs {before}"
    );
    assert_eq!(voices(&audio), 1);
}

fn filtered_patch() -> AudioPatch {
    let def = AudioPatchDef {
        version: 1,
        name: "hum".into(),
        bus: Some("Sfx".into()),
        parameters: vec![PatchParamDef {
            name: "open".into(),
            min: 0.0,
            max: 1.0,
            smoothing_secs: 0.0,
            binding: None,
        }],
        layers: vec![PatchLayerDef {
            name: "hum".into(),
            sound: Some("hum".into()),
            keys: vec![],
            parameter: None,
            volume_db: 0.0,
            volume_curves: vec![],
            pitch_curves: vec![],
            filter_curves: vec![PatchCurveDef {
                parameter: "open".into(),
                points: vec![(0.0, 400.0), (1.0, 20_000.0)],
            }],
            one_shot: None,
            granular: None,
        }],
    };
    // 200 Hz body plus an 8 kHz edge the low-pass must remove.
    let len = RATE as usize / 2;
    let samples: Vec<f32> = (0..len)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            (t * 200.0 * std::f32::consts::TAU).sin() * 0.2
                + (t * 8_000.0 * std::f32::consts::TAU).sin() * 0.2
        })
        .collect();
    let hum = SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("hum clip"));
    let mut layer_sounds = HashMap::new();
    layer_sounds.insert("hum".to_string(), hum);
    AudioPatch {
        def,
        layer_sounds,
        key_sounds: HashMap::new(),
    }
}

/// AUDIO_PATCH_FILTER: a layer's `filter_curves` low-pass what is heard.
#[test]
fn patch_filter_curves_change_the_spectrum() {
    let mut audio = headless();
    let id = audio.spawn_patch(filtered_patch()).expect("patch spawns");

    audio.patch_set_param(id, "open", 1.0).expect("param");
    let _ = run(&mut audio, 0.2);
    let open = run(&mut audio, 0.2);

    audio.patch_set_param(id, "open", 0.0).expect("param");
    let _ = run(&mut audio, 0.2);
    let closed = run(&mut audio, 0.2);

    let (high_open, high_closed) = (tone_level(&open, 8_000.0), tone_level(&closed, 8_000.0));
    let (low_open, low_closed) = (tone_level(&open, 200.0), tone_level(&closed, 200.0));
    assert!(
        high_open > 0.03,
        "the open filter passes the 8 kHz edge: {high_open}"
    );
    assert!(
        db(high_closed / high_open) < -20.0,
        "a 400 Hz cutoff removes the 8 kHz edge: {high_closed} vs {high_open} ({:.1} dB)",
        db(high_closed / high_open)
    );
    assert!(
        db(low_closed / low_open).abs() < 3.0,
        "the 200 Hz body passes both settings: {low_closed} vs {low_open}"
    );
    audio.despawn_patch(id);
    let _ = run(&mut audio, 0.1);
    let after = run(&mut audio, 0.1);
    assert!(
        tone_level(&after, 200.0) < 1e-4 && tone_level(&after, 8_000.0) < 1e-4,
        "a despawned patch is silent (its filter track went with it)"
    );
}

/// `start_asset` returns the voice id, so a host can stop what it started.
#[test]
fn start_asset_returns_a_stoppable_voice() {
    let mut audio = headless();
    let id = audio
        .start_asset(&tone(220.0, 0.3), looping_on("Music"))
        .expect("starts");
    assert_eq!(voices(&audio), 1);
    assert!(tone_level(&run(&mut audio, 0.2), 220.0) > 0.03, "audible");
    audio.stop_voice(id, Duration::from_millis(10));
    let _ = run(&mut audio, 0.1);
    assert!(tone_level(&run(&mut audio, 0.1), 220.0) < 1e-4, "stopped");
    assert_eq!(voices(&audio), 0, "the stopped voice is pruned");
}

/// A one-layer patch (no keys, no curves) playing a 220 Hz tone at `volume_db`.
fn plain_patch(volume_db: f32) -> AudioPatch {
    let def = AudioPatchDef {
        version: 1,
        name: "plain".into(),
        bus: Some("Sfx".into()),
        parameters: vec![],
        layers: vec![PatchLayerDef {
            name: "hum".into(),
            sound: Some("hum".into()),
            keys: vec![],
            parameter: None,
            volume_db,
            volume_curves: vec![],
            pitch_curves: vec![],
            filter_curves: vec![],
            one_shot: None,
            granular: None,
        }],
    };
    let mut layer_sounds = HashMap::new();
    layer_sounds.insert("hum".to_string(), tone(220.0, 0.3));
    AudioPatch {
        def,
        layer_sounds,
        key_sounds: HashMap::new(),
    }
}

/// A persistent patch loop must be heard again without the host replacing its patch id.
#[test]
fn loop_restarts_after_output_resume() {
    let mut audio = headless();
    let id = audio.spawn_patch(plain_patch(-6.0)).expect("patch spawns");
    let before = tone_level(&run(&mut audio, 0.2), 220.0);
    assert!(before > 0.02, "the original loop is audible: {before}");
    for _ in 0..3 {
        audio.suspend_output();
        assert!(audio.resume_output(), "the offline output resumes");
        let _ = run(&mut audio, 0.2);
        let after = tone_level(&run(&mut audio, 0.2), 220.0);
        assert!(
            db(after / before).abs() < 1.0,
            "the same patch loop restarts at its level: {after} vs {before}"
        );
    }
    audio.despawn_patch(id);
    let _ = run(&mut audio, 0.2);
    assert!(tone_level(&run(&mut audio, 0.2), 220.0) < 1e-4);
}

#[test]
fn rapid_output_cycles_without_rendering_reclaim_tracks() {
    let mut audio = headless();
    let patch = audio.spawn_patch(plain_patch(-6.0)).expect("patch");
    let voice = audio
        .start_persistent_loop(&tone(660.0, 0.3), PlaybackParams::default())
        .expect("loop");
    for _ in 0..20 {
        audio.suspend_output();
        assert!(audio.resume_output(), "retired tracks must not exhaust the mixer");
    }
    assert_eq!(audio.suspend_count(), 20);
    assert_eq!(audio.resume_count(), 20);
    assert_eq!(audio.persistent_loop_count(), 1);
    assert_eq!(audio.persistent_patch_count(), 1);
    assert!(audio.voice_alive(voice));
    let _ = run(&mut audio, 0.2);
    let samples = run(&mut audio, 0.2);
    assert!(tone_level(&samples, 220.0) > 0.03);
    assert!(tone_level(&samples, 660.0) > 0.05);
    audio.despawn_patch(patch);
    audio.stop_voice(voice, Duration::ZERO);
}

/// A non-keyed layer starts at its authored `volume_db`, not at 0 dB.
#[test]
fn non_keyed_layer_honours_volume_db() {
    let level = |volume_db: f32| {
        let mut audio = headless();
        audio.spawn_patch(plain_patch(volume_db)).expect("patch spawns");
        let _ = run(&mut audio, 0.2);
        tone_level(&run(&mut audio, 0.2), 220.0)
    };
    let (loud, quiet) = (level(0.0), level(-12.0));
    assert!(loud > 0.03, "the reference layer is audible: {loud}");
    assert!(
        (db(quiet / loud) + 12.0).abs() < 1.0,
        "-12 dB layer: {quiet} vs {loud} ({:.1} dB)",
        db(quiet / loud)
    );
}

/// Popping a snapshot puts a bus's authored filter back, not a 20 kHz cutoff.
#[test]
fn popping_a_snapshot_restores_the_layout_filter() {
    use regia_audio::{AudioSnapshot, BusDef, BusEffect, BusLayout};
    let bus = |name: &str, parent: Option<&str>, effects: Vec<BusEffect>| BusDef {
        name: name.into(),
        parent: parent.map(str::to_string),
        volume_db: 0.0,
        mute: false,
        effects,
        sends: vec![],
        bypass_medium: false,
    };
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        buses: BusLayout {
            buses: vec![
                bus("Master", None, vec![]),
                bus(
                    "Radio",
                    Some("Master"),
                    vec![BusEffect::HighPass {
                        cutoff_hz: 100.0,
                        resonance: 0.0,
                    }],
                ),
            ],
            send_buses: vec![],
            ..BusLayout::default()
        },
        ..AudioSettings::default()
    });
    audio.register_snapshot(AudioSnapshot::new("Duck").with_bus_volume("Master", -3.0));
    audio
        .play_asset(&tone(440.0, 0.3), looping_on("Radio"))
        .expect("enqueue");
    audio.drain();
    let _ = run(&mut audio, 0.3);
    let before = tone_level(&run(&mut audio, 0.2), 440.0);

    audio.push_snapshot("Duck", Duration::ZERO).expect("push");
    let _ = run(&mut audio, 0.2);
    audio.pop_snapshot(Duration::ZERO).expect("pop");
    let _ = run(&mut audio, 0.3);
    let after = tone_level(&run(&mut audio, 0.2), 440.0);
    // A 100 Hz high-pass passes the 440 Hz tone; restoring it as a 20 kHz high-pass would not.
    assert!(before > 0.03, "the authored high-pass passes the tone: {before}");
    assert!(
        db(after / before).abs() < 3.0,
        "the layout filter is back after the pop: {after} vs {before}"
    );
}

#[test]
fn filtered_patch_keeps_parameter_state_across_resume() {
    let mut audio = headless();
    let id = audio.spawn_patch(filtered_patch()).expect("patch");
    audio.patch_set_param(id, "open", 0.4).expect("set parameter");
    let _ = run(&mut audio, 1.0);
    let before = run(&mut audio, 0.2);
    audio.suspend_output();
    assert!(audio.resume_output());
    let _ = run(&mut audio, 0.2);
    let after = run(&mut audio, 0.2);
    for hz in [200.0, 8_000.0] {
        let ratio = tone_level(&after, hz) / tone_level(&before, hz);
        assert!(
            db(ratio).abs() < 1.0,
            "retained filter state at {hz}: {} dB",
            db(ratio)
        );
    }
    audio
        .patch_set_param(id, "open", 0.0)
        .expect("original id is still editable");
    let _ = run(&mut audio, 0.3);
    let closed = run(&mut audio, 0.2);
    assert!(db(tone_level(&closed, 8_000.0) / tone_level(&after, 8_000.0)) < -20.0);
}

#[test]
fn patch_created_or_despawned_while_suspended_obeys_ownership() {
    let mut audio = headless();
    let removed = audio.spawn_patch(plain_patch(-6.0)).expect("patch");
    let _ = run(&mut audio, 0.2);
    audio.suspend_output();
    audio.despawn_patch(removed);
    let added = audio.spawn_patch(filtered_patch()).expect("deferred patch");
    audio
        .patch_set_param(added, "open", 1.0)
        .expect("target while suspended");
    assert!(audio.resume_output());
    let _ = run(&mut audio, 0.4);
    let samples = run(&mut audio, 0.2);
    assert!(tone_level(&samples, 220.0) < 1e-3, "removed loop stays gone");
    assert!(
        tone_level(&samples, 8_000.0) > 0.03,
        "new patch and target survive"
    );
    audio.despawn_patch(added);
    audio.suspend_output();
    assert!(audio.resume_output());
    let _ = run(&mut audio, 0.2);
    assert!(run(&mut audio, 0.2).iter().all(|s| s.abs() < 1e-4));
}

#[test]
fn keyed_patch_recovers_at_the_retained_crossfade_before_the_next_tick() {
    let mut patch = plain_patch(0.0);
    patch.def.parameters.push(PatchParamDef {
        name: "drive".into(),
        min: 0.0,
        max: 1.0,
        smoothing_secs: 0.01,
        binding: None,
    });
    let layer = &mut patch.def.layers[0];
    layer.sound = None;
    layer.parameter = Some("drive".into());
    layer.keys = vec![
        regia_audio::PatchKeyDef {
            value: 0.0,
            sound: "low".into(),
            gain_db: 0.0,
            pitch: 1.0,
        },
        regia_audio::PatchKeyDef {
            value: 1.0,
            sound: "high".into(),
            gain_db: -6.0,
            pitch: 1.0,
        },
    ];
    patch.layer_sounds.clear();
    patch.key_sounds.insert(("hum".into(), 0), tone(220.0, 0.3));
    patch.key_sounds.insert(("hum".into(), 1), tone(660.0, 0.3));
    let mut audio = headless();
    let id = audio.spawn_patch(patch).expect("keyed patch");
    audio.patch_set_param(id, "drive", 1.0).expect("high key");
    let _ = run(&mut audio, 0.2);
    let before = run(&mut audio, 0.2);
    audio.suspend_output();
    assert!(audio.resume_output());
    // No audio.tick: the replacement handles must start at the saved weights, not all keys on.
    let after = audio.render_offline(9_600).expect("offline");
    assert!(tone_level(&after, 220.0) < 1e-3, "the low key stayed muted");
    let ratio = tone_level(&after, 660.0) / tone_level(&before, 660.0);
    assert!(
        db(ratio).abs() < 1.0,
        "the high key kept its gain: {} dB",
        db(ratio)
    );
}

#[test]
fn patch_asset_defaults_match_before_and_after_recovery() {
    assert_patch_asset_defaults(false);
}

#[test]
fn keyed_patch_asset_defaults_match_before_and_after_recovery() {
    assert_patch_asset_defaults(true);
}

fn assert_patch_asset_defaults(keyed: bool) {
    let mut patch = plain_patch(0.0);
    let asset = tone(220.0, 0.3)
        .with_default_volume_db(-6.0)
        .with_loudness_gain_db(-3.0)
        .with_default_pitch(0.5);
    if keyed {
        patch.layer_sounds.clear();
        patch.key_sounds.insert(("hum".into(), 0), asset);
        patch.def.layers[0].sound = None;
        patch.def.layers[0].parameter = Some("drive".into());
        patch.def.layers[0].keys.push(regia_audio::PatchKeyDef {
            value: 0.0,
            sound: "tone".into(),
            gain_db: 0.0,
            pitch: 1.0,
        });
    } else {
        patch.layer_sounds.insert("hum".into(), asset);
    }
    patch.def.parameters.push(PatchParamDef {
        name: "drive".into(),
        min: 0.0,
        max: 1.0,
        smoothing_secs: 0.01,
        binding: None,
    });
    patch.def.layers[0].volume_curves.push(PatchCurveDef {
        parameter: "drive".into(),
        points: vec![(0.0, -12.0), (1.0, 0.0)],
    });
    patch.def.layers[0].pitch_curves.push(PatchCurveDef {
        parameter: "drive".into(),
        points: vec![(0.0, 1.0), (1.0, 2.0)],
    });
    let mut audio = headless();
    let id = audio.spawn_patch(patch).expect("patch");
    audio.patch_set_param(id, "drive", 1.0).expect("drive");
    let _ = run(&mut audio, 0.2);
    let before = tone_level(&run(&mut audio, 0.2), 220.0);
    assert!(
        before > 0.03,
        "2x curve times 0.5x asset is the original tone: {before}"
    );
    let expected = 0.15 * 10.0_f32.powf(-9.0 / 20.0);
    assert!(
        (before - expected).abs() < 0.001,
        "asset volume and loudness offsets compose: {before} vs {expected}"
    );
    audio.suspend_output();
    assert!(audio.resume_output());
    let _ = run(&mut audio, 0.2);
    let after = tone_level(&run(&mut audio, 0.2), 220.0);
    assert!(
        db(after / before).abs() < 1.0,
        "asset defaults stay composed across recovery"
    );
}

fn mix_peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()))
}

/// A render-out take on a suspended output (every window hidden, or a software
/// adapter) still mixes a patch spawned during the take (review AU1).
#[test]
fn a_patch_plays_in_a_take_on_a_suspended_output() {
    let mut audio = headless();
    audio.suspend_output();
    audio.begin_mixdown(&MixdownSettings::default()).expect("take");
    audio.spawn_patch(plain_patch(-6.0)).expect("patch");
    let mut peak = 0.0f32;
    for _ in 0..12 {
        audio.tick(FRAME);
        let block = audio.render_mixdown(800).expect("block");
        peak = peak.max(mix_peak(&block.mix));
    }
    assert!(audio.end_mixdown());
    assert!(
        peak > 0.05,
        "a patch spawned in a suspended take is silent: peak {peak}"
    );
}

/// Suspending the device output mid-take must not stop patch layers the take is mixing.
#[test]
fn a_patch_stays_audible_when_output_suspends_mid_take() {
    let mut audio = headless();
    audio.begin_mixdown(&MixdownSettings::default()).expect("take");
    audio.spawn_patch(plain_patch(-6.0)).expect("patch");
    let mut before = 0.0f32;
    for _ in 0..6 {
        audio.tick(FRAME);
        let block = audio.render_mixdown(800).expect("block");
        before = before.max(mix_peak(&block.mix));
    }
    assert!(before > 0.05, "the take hears the patch before suspend: {before}");
    audio.suspend_output();
    assert!(audio.output_suspended());
    // `stop` fades over 50 ms. A cleared layer is silent after that; a layer the take
    // kept playing is still the tone. Measure only the later blocks.
    let mut after = 0.0f32;
    for step in 0..24 {
        audio.tick(FRAME);
        let block = audio.render_mixdown(800).expect("block");
        if step >= 18 {
            after = after.max(mix_peak(&block.mix));
        }
    }
    assert!(audio.end_mixdown());
    assert!(
        after > 0.05,
        "suspend mid-take silenced the patch: peak {after} (before {before})"
    );
}

/// A host-owned loop started during a suspended take plays in that take (AU1).
#[test]
fn a_persistent_loop_plays_in_a_take_on_a_suspended_output() {
    let mut audio = headless();
    audio.suspend_output();
    audio.begin_mixdown(&MixdownSettings::default()).expect("take");
    audio
        .start_persistent_loop(&tone(440.0, 0.3), PlaybackParams::default())
        .expect("loop");
    let mut peak = 0.0f32;
    for _ in 0..12 {
        audio.tick(FRAME);
        let block = audio.render_mixdown(800).expect("block");
        peak = peak.max(mix_peak(&block.mix));
    }
    assert!(audio.end_mixdown());
    assert!(
        peak > 0.05,
        "a persistent loop started in a suspended take is silent: peak {peak}"
    );
}

/// An unknown patch bus is an error while the mixer is down, same as on a live output (AU2).
#[test]
fn spawn_patch_while_suspended_rejects_an_unknown_bus() {
    let mut audio = headless();
    audio.suspend_output();
    let mut patch = plain_patch(-6.0);
    patch.def.bus = Some("NoSuchBus".into());
    let error = audio.spawn_patch(patch).expect_err("unknown bus");
    assert!(
        matches!(error, AudioError::UnknownBus(ref bus) if bus == "NoSuchBus"),
        "suspended spawn must reject the bus, got {error:?}"
    );
    assert_eq!(audio.persistent_patch_count(), 0);
}
