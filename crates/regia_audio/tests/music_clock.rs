// SPDX-License-Identifier: MPL-2.0
//! Music on the audio clock (ADR-2517), through the offline mixer: stingers and transitions land on
//! exact samples whatever the render step, and gameplay hears about every beat. Sounds are constant
//! levels, so the sample where one starts or stops can be read straight off the output.

use std::time::Duration;

use regia_app::{App, TimeUpdateStrategy};
use regia_audio::{
    Audio, AudioPlugin, AudioSettings, BusLayout, MUSIC_SCHEDULE_LEAD_SECS, MusicAsset, MusicBeat,
    MusicSegment, MusicStem, MusicStinger, MusicSync, MusicTransitionRequest, MusicTransitionRule,
    SoundAsset, SoundClip,
};
use regia_ecs::prelude::*;

const RATE: u32 = 48_000;
/// 120 bpm: a beat is 24 000 samples, a 4/4 bar 96 000.
const BEAT: usize = 24_000;
const BAR: usize = BEAT * 4;

/// A headless mixer with the Music bus at unity (it is −3 dB in the default tree), so the output
/// reads the levels of the sounds.
fn settings() -> AudioSettings {
    let mut buses = BusLayout::default_tree();
    for bus in &mut buses.buses {
        if bus.name == "Music" {
            bus.volume_db = 0.0;
        }
    }
    AudioSettings {
        headless: true,
        buses,
        ..AudioSettings::default()
    }
}

fn headless() -> Audio {
    Audio::new(&settings())
}

/// A constant level, `frames` long.
fn level(value: f32, frames: usize) -> SoundAsset {
    SoundAsset::from_clip(&SoundClip::new(vec![value; frames], RATE).expect("a valid clip"))
}

fn stem(name: &str, value: f32) -> MusicStem {
    MusicStem {
        name: name.into(),
        asset: level(value, BAR),
        base_db: 0.0,
        intensity: Vec::new(),
    }
}

/// A one-bar loop at `value` on a 120 bpm 4/4 grid.
fn track(name: &str, value: f32) -> MusicAsset {
    MusicAsset::from_stems(name, 120.0, 4, vec![stem("a", value)])
}

fn stinger(name: &str, value: f32, sync: MusicSync) -> MusicStinger {
    MusicStinger {
        name: name.into(),
        asset: level(value, 2_000),
        volume_db: 0.0,
        sync,
    }
}

/// Renders at least `frames` left-channel samples in steps of `step`, ticking the audio between
/// steps. Everything rendered is returned, so consecutive calls concatenate to the output's samples.
fn render(audio: &mut Audio, frames: usize, step: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames + step);
    while out.len() < frames {
        audio.tick(step as f64 / f64::from(RATE));
        let block = audio.render_offline(step).expect("the offline backend");
        out.extend(block.iter().step_by(2).copied());
    }
    out
}

fn first_at_least(samples: &[f32], value: f32) -> Option<usize> {
    samples.iter().position(|s| *s >= value)
}

fn assert_near(actual: f32, expected: f32, what: &str) {
    assert!(
        (actual - expected).abs() < 1e-3,
        "{what}: {actual}, expected {expected}"
    );
}

#[test]
fn one_second_at_120_bpm_is_beat_index_two() {
    let mut audio = headless();
    audio.play_music(&track("t", 0.0), Duration::ZERO).expect("plays");
    let _ = render(&mut audio, RATE as usize, 800);
    // This thread reads the clock where the mixer's last output step started: kira publishes a
    // clock to its handles when a step starts (`audio.music_state`: "up to one output callback
    // behind the mixer"). One second of output in 800-frame steps reads 47 200 frames, still in
    // beat 1; the step that starts at sample 48 000 makes the read one second.
    let _ = render(&mut audio, 800, 800);
    let read = audio.music_state().clock_secs.expect("a clock");
    assert!(
        (read - 1.0).abs() < 1e-9,
        "the clock reads {read} s, not one second"
    );
    // The kira clock's tick at t = 1 is 1. The beat index `clock_tick` reports is 2.
    assert_eq!(audio.music_state().beat_index, Some(2));
}

#[test]
fn a_stinger_starts_exactly_on_the_next_bar_sample() {
    // Render steps that divide neither a bar nor kira's 128-frame block, to prove the start is not
    // rounded to any of them.
    for step in [800usize, 137, 1_024] {
        let mut audio = headless();
        let mut asset = track("t", 0.0);
        asset.stingers.push(stinger("hit", 0.5, MusicSync::NextBar));
        audio.play_music(&asset, Duration::ZERO).expect("plays");
        let mut heard = render(&mut audio, BAR / 2, step);
        let schedule = audio.music_stinger("hit", None).expect("schedules");
        assert_eq!(schedule.at_frame, BAR as u64, "step {step}");
        assert_eq!((schedule.bar, schedule.beat), (1, 0), "step {step}");
        assert!(!schedule.noop);
        heard.extend(render(&mut audio, BAR, step));
        assert_eq!(first_at_least(&heard, 0.25), Some(BAR), "step {step}");
        assert_near(heard[BAR], 0.5, "the stinger's first sample");
    }
}

#[test]
fn a_stinger_can_land_on_the_next_beat() {
    let mut audio = headless();
    let mut asset = track("t", 0.0);
    asset.stingers.push(stinger("tick", 0.5, MusicSync::NextBar));
    audio.play_music(&asset, Duration::ZERO).expect("plays");
    let mut heard = render(&mut audio, BEAT / 2, 800);
    // The next beat, overriding the stinger's own bar sync.
    let schedule = audio
        .music_stinger("tick", Some(MusicSync::NextBeat))
        .expect("schedules");
    assert_eq!(schedule.at_frame, BEAT as u64);
    assert_eq!((schedule.bar, schedule.beat), (0, 1));
    heard.extend(render(&mut audio, BEAT, 800));
    assert_eq!(first_at_least(&heard, 0.25), Some(BEAT));
}

#[test]
fn a_tempo_without_whole_samples_per_beat_lands_on_the_nearest_sample() {
    // 132 bpm: a 4/4 bar is 87 272.73 samples.
    let mut audio = headless();
    let mut asset = MusicAsset::from_stems("t", 132.0, 4, vec![stem("a", 0.0)]);
    asset.stingers.push(stinger("hit", 0.5, MusicSync::NextBar));
    audio.play_music(&asset, Duration::ZERO).expect("plays");
    let mut heard = render(&mut audio, 40_000, 800);
    let schedule = audio.music_stinger("hit", None).expect("schedules");
    assert_eq!(schedule.at_frame, 87_273);
    heard.extend(render(&mut audio, 60_000, 800));
    assert_eq!(first_at_least(&heard, 0.25), Some(87_273));
}

#[test]
fn a_boundary_inside_the_schedule_lead_goes_to_the_next_one() {
    // The mixer may not get the sound in time for a boundary this close.
    let lead = (MUSIC_SCHEDULE_LEAD_SECS * f64::from(RATE)) as usize;
    let mut audio = headless();
    let mut asset = track("t", 0.0);
    asset.stingers.push(stinger("hit", 0.5, MusicSync::NextBar));
    audio.play_music(&asset, Duration::ZERO).expect("plays");
    let _ = render(&mut audio, BAR - lead / 2, 100);
    audio.tick(0.001);
    let schedule = audio.music_stinger("hit", None).expect("schedules");
    assert_eq!(schedule.at_frame, (2 * BAR) as u64);
    assert_eq!(audio.music_state().late_starts, 0);
}

#[test]
fn asking_for_a_stinger_with_no_track_or_a_wrong_name_fails() {
    let mut audio = headless();
    assert!(audio.music_stinger("hit", None).is_err(), "nothing is playing");
    audio.play_music(&track("t", 0.0), Duration::ZERO).expect("plays");
    let error = audio.music_stinger("nope", None).err().map(|e| e.to_string());
    assert_eq!(
        error.as_deref(),
        Some("audio cue or track `nope` is not registered")
    );
}

#[test]
fn a_next_bar_transition_switches_at_the_bar_boundary() {
    for step in [800usize, 137] {
        let mut audio = headless();
        audio.register_music(track("calm", 0.4));
        audio
            .play_music(&track("combat", 0.2), Duration::ZERO)
            .expect("plays");
        let mut heard = render(&mut audio, BAR / 2, step);
        assert_eq!(audio.current_music(), Some("combat"));
        let schedule = audio
            .music_transition("calm", MusicTransitionRequest::default())
            .expect("schedules");
        assert_eq!(schedule.at_frame, BAR as u64, "step {step}");
        assert_eq!(schedule.sync, MusicSync::NextBar);
        assert_eq!(audio.music_state().pending.map(|p| p.to).as_deref(), Some("calm"));
        heard.extend(render(&mut audio, BAR, step));
        // The old track up to the last sample of the bar; the new one from the boundary on. The
        // old one's 10 ms declick has ended 480 samples later.
        assert_near(heard[BAR - 1], 0.2, "the last sample before the bar");
        assert!(heard[BAR] >= 0.4 - 1e-3, "step {step}: {} on the bar", heard[BAR]);
        assert_near(heard[BAR + 480], 0.4, "after the declick");
        assert_near(*heard.last().expect("samples"), 0.4, "the new track");
        assert_eq!(audio.current_music(), Some("calm"), "step {step}");
        assert_eq!(audio.music_state().pending, None);
        assert_eq!(audio.music_state().late_starts, 0);
    }
}

#[test]
fn a_next_beat_transition_switches_at_the_beat() {
    let mut audio = headless();
    audio.register_music(track("calm", 0.4));
    audio
        .play_music(&track("combat", 0.2), Duration::ZERO)
        .expect("plays");
    let mut heard = render(&mut audio, BEAT + 100, 800);
    let schedule = audio
        .music_transition(
            "calm",
            MusicTransitionRequest {
                sync: Some(MusicSync::NextBeat),
                ..Default::default()
            },
        )
        .expect("schedules");
    assert_eq!(schedule.at_frame, 2 * BEAT as u64);
    heard.extend(render(&mut audio, BEAT * 2, 800));
    assert_near(heard[2 * BEAT - 1], 0.2, "before the beat");
    assert_near(heard[2 * BEAT + 480], 0.4, "after the beat");
}

#[test]
fn an_immediate_transition_starts_after_the_lead() {
    let lead = (MUSIC_SCHEDULE_LEAD_SECS * f64::from(RATE)).round() as usize;
    let mut audio = headless();
    audio.register_music(track("calm", 0.4));
    audio
        .play_music(&track("combat", 0.2), Duration::ZERO)
        .expect("plays");
    let mut heard = render(&mut audio, 8_000, 800);
    // What this thread reads of the clock is one render step behind the mixer.
    let read = audio.music_state().clock_secs.expect("a clock");
    let schedule = audio
        .music_transition(
            "calm",
            MusicTransitionRequest {
                sync: Some(MusicSync::Immediate),
                ..Default::default()
            },
        )
        .expect("schedules");
    let expected = (read * f64::from(RATE)).round() as usize + lead;
    assert_eq!(schedule.at_frame, expected as u64);
    heard.extend(render(&mut audio, BEAT, 800));
    assert_near(heard[expected - 1], 0.2, "before");
    assert_near(heard[expected + 480], 0.4, "after");
}

#[test]
fn an_end_of_segment_transition_waits_for_the_segment() {
    let mut audio = headless();
    audio.register_music(track("calm", 0.4));
    let mut combat = track("combat", 0.2);
    combat.segment_bars = Some(2);
    combat.transitions.push(MusicTransitionRule {
        to: "calm".into(),
        sync: MusicSync::EndOfSegment,
        crossfade_secs: 0.0,
        segment: None,
    });
    audio.play_music(&combat, Duration::ZERO).expect("plays");
    let mut heard = render(&mut audio, BAR + 800, 800);
    let schedule = audio
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("schedules");
    assert_eq!(schedule.sync, MusicSync::EndOfSegment);
    assert_eq!(schedule.at_frame, 2 * BAR as u64);
    heard.extend(render(&mut audio, BAR + 1_000, 800));
    assert_near(heard[2 * BAR - 1], 0.2, "before the segment ends");
    assert_near(heard[2 * BAR + 480], 0.4, "after it");
}

#[test]
fn a_track_without_a_segment_length_ends_its_segment_at_the_stem_length() {
    let mut audio = headless();
    audio.register_music(track("calm", 0.4));
    // A 3-bar stem and no `segment_bars`.
    let long = MusicStem {
        asset: level(0.2, 3 * BAR),
        ..stem("a", 0.2)
    };
    let combat = MusicAsset::from_stems("combat", 120.0, 4, vec![long]);
    audio.play_music(&combat, Duration::ZERO).expect("plays");
    let _ = render(&mut audio, 800, 800);
    let schedule = audio
        .music_transition(
            "calm",
            MusicTransitionRequest {
                sync: Some(MusicSync::EndOfSegment),
                ..Default::default()
            },
        )
        .expect("schedules");
    assert_eq!(schedule.at_frame, 3 * BAR as u64);
}

#[test]
fn a_crossfade_is_equal_power_and_starts_at_the_switch() {
    let fade_secs = 0.5;
    let fade = (fade_secs * f64::from(RATE)) as usize;
    let mut audio = headless();
    audio.register_music(track("calm", 0.4));
    audio
        .play_music(&track("combat", 0.4), Duration::ZERO)
        .expect("plays");
    let mut heard = render(&mut audio, BAR / 2, 800);
    audio
        .music_transition(
            "calm",
            MusicTransitionRequest {
                crossfade_secs: Some(fade_secs),
                ..Default::default()
            },
        )
        .expect("schedules");
    heard.extend(render(&mut audio, BAR, 800));
    assert_near(heard[BAR - 1], 0.4, "before the fade");
    // Halfway each voice is at 1/√2: equal power, so two uncorrelated tracks hold their level.
    // These two identical levels sum to √2 of one.
    assert_near(
        heard[BAR + fade / 2],
        0.4 * std::f32::consts::SQRT_2,
        "the middle of the fade",
    );
    assert_near(heard[BAR + fade + 10], 0.4, "after the fade");
}

#[test]
fn a_transition_segment_crossfades_equal_power_into_the_segment() {
    // Same midpoint as a stem crossfade: both levels 0.4, so the middle is 0.4 * √2.
    // The segment is longer than the fade, so the next track has not started yet.
    let fade_secs = 0.5;
    let fade = (fade_secs * f64::from(RATE)) as usize;
    let mut audio = headless();
    audio.register_music(track("calm", 0.4));
    let mut combat = track("combat", 0.4);
    combat.transitions.push(MusicTransitionRule {
        to: "calm".into(),
        sync: MusicSync::NextBar,
        crossfade_secs: fade_secs,
        segment: Some(MusicSegment {
            asset: level(0.4, 2 * BEAT),
            volume_db: 0.0,
        }),
    });
    audio.play_music(&combat, Duration::ZERO).expect("plays");
    let mut heard = render(&mut audio, BAR / 2, 800);
    audio
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("schedules");
    heard.extend(render(&mut audio, BAR, 800));
    assert_near(heard[BAR - 1], 0.4, "before the fade");
    assert_near(
        heard[BAR + fade / 2],
        0.4 * std::f32::consts::SQRT_2,
        "the middle of the fade into the segment",
    );
    assert_near(heard[BAR + fade + 10], 0.4, "the segment after the fade");
}

#[test]
fn a_short_transition_segment_silences_the_old_track_before_the_new_one() {
    // The segment is 0.5 s and the crossfade is 2 s. The old track must be silent on the
    // new track's first sample (the segment's end), not still fading out.
    let mut audio = headless();
    audio.register_music(track("calm", 0.5));
    let mut combat = track("combat", 0.2);
    combat.transitions.push(MusicTransitionRule {
        to: "calm".into(),
        sync: MusicSync::NextBar,
        crossfade_secs: 2.0,
        segment: Some(MusicSegment {
            asset: level(0.4, BEAT),
            volume_db: 0.0,
        }),
    });
    audio.play_music(&combat, Duration::ZERO).expect("plays");
    let mut heard = render(&mut audio, BAR / 2, 800);
    audio
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("schedules");
    heard.extend(render(&mut audio, BAR / 2 + BEAT + 800, 800));
    assert_near(
        heard[BAR + BEAT],
        0.5,
        "the new track's first sample, old track silent",
    );
}

#[test]
fn a_transition_segment_plays_between_the_tracks() {
    // A two-beat segment between the old track and the new one.
    let mut audio = headless();
    audio.register_music(track("calm", 0.4));
    let mut combat = track("combat", 0.2);
    combat.transitions.push(MusicTransitionRule {
        to: "calm".into(),
        sync: MusicSync::NextBar,
        crossfade_secs: 0.0,
        segment: Some(MusicSegment {
            asset: level(0.3, 2 * BEAT),
            volume_db: 0.0,
        }),
    });
    audio.play_music(&combat, Duration::ZERO).expect("plays");
    let mut heard = render(&mut audio, BAR / 2, 800);
    let schedule = audio
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("schedules");
    assert_eq!(schedule.at_frame, BAR as u64);
    let pending = audio.music_state().pending.expect("on its way");
    assert!(pending.segment);
    assert_eq!(pending.switch_secs, 2.0);
    assert_eq!(pending.start_secs, 3.0, "the switch plus the segment");
    heard.extend(render(&mut audio, BAR + BEAT, 800));
    assert_near(heard[BAR - 1], 0.2, "the old track");
    assert_near(heard[BAR + 480], 0.3, "the segment");
    assert_near(heard[BAR + 2 * BEAT - 1], 0.3, "the segment's last sample");
    assert_near(heard[BAR + 2 * BEAT], 0.4, "the new track on its first sample");
    assert_eq!(audio.current_music(), Some("calm"));
}

#[test]
fn the_new_track_keeps_its_own_grid_after_a_transition() {
    let mut audio = headless();
    let mut calm = MusicAsset::from_stems("calm", 60.0, 3, vec![stem("a", 0.0)]);
    calm.stingers.push(stinger("chime", 0.5, MusicSync::NextBar));
    audio.register_music(calm);
    audio
        .play_music(&track("combat", 0.0), Duration::ZERO)
        .expect("plays");
    let mut heard = render(&mut audio, BAR / 2, 800);
    audio
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("schedules");
    heard.extend(render(&mut audio, BAR, 800));
    assert_eq!(audio.current_music(), Some("calm"));
    let state = audio.music_state();
    assert_eq!(state.bpm, Some(60.0));
    assert_eq!(state.beats_per_bar, Some(3));
    // 60 bpm in 3/4: a bar is 3 s, from the track's start at the 2 s mark.
    let schedule = audio.music_stinger("chime", None).expect("schedules");
    assert_eq!(schedule.at_frame, BAR as u64 + 3 * u64::from(RATE));
    heard.extend(render(&mut audio, 3 * RATE as usize, 800));
    assert_eq!(first_at_least(&heard, 0.25), Some(BAR + 3 * RATE as usize));
}

#[test]
fn asking_for_the_playing_or_pending_track_changes_nothing() {
    let mut audio = headless();
    audio.register_music(track("combat", 0.2));
    audio.register_music(track("calm", 0.4));
    audio
        .play_music(&track("combat", 0.2), Duration::ZERO)
        .expect("plays");
    let _ = render(&mut audio, BEAT, 800);
    let same = audio
        .music_transition("combat", MusicTransitionRequest::default())
        .expect("no-op");
    assert!(same.noop);
    assert_eq!(audio.music_state().pending, None);
    let first = audio
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("schedules");
    assert!(!first.noop);
    let again = audio
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("no-op");
    assert!(again.noop);
    assert_eq!(again.at_frame, first.at_frame);
}

#[test]
fn a_transition_can_be_replaced_or_taken_back_before_its_switch() {
    let mut audio = headless();
    audio.register_music(track("combat", 0.2));
    audio.register_music(track("calm", 0.4));
    audio.register_music(track("menu", 0.6));
    audio
        .play_music(&track("combat", 0.2), Duration::ZERO)
        .expect("plays");
    let mut heard = render(&mut audio, BEAT, 800);
    audio
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("schedules");
    heard.extend(render(&mut audio, 800, 800));
    audio
        .music_transition("menu", MusicTransitionRequest::default())
        .expect("replaces");
    assert_eq!(audio.music_state().pending.map(|p| p.to).as_deref(), Some("menu"));
    heard.extend(render(&mut audio, 800, 800));
    // Back to the track that never stopped: the stop is withdrawn.
    let back = audio
        .music_transition("combat", MusicTransitionRequest::default())
        .expect("takes back");
    assert!(back.noop);
    assert_eq!(audio.music_state().pending, None);
    heard.extend(render(&mut audio, 2 * BAR, 800));
    assert_eq!(audio.current_music(), Some("combat"));
    assert_near(*heard.last().expect("samples"), 0.2, "still the old track");
    assert!(
        heard.iter().all(|s| *s < 0.21),
        "neither of the taken-back tracks was ever heard"
    );
}

#[test]
fn a_request_after_the_switch_waits_for_the_next_track() {
    let mut audio = headless();
    audio.register_music(track("calm", 0.4));
    audio.register_music(track("menu", 0.6));
    let mut combat = track("combat", 0.2);
    combat.transitions.push(MusicTransitionRule {
        to: "calm".into(),
        sync: MusicSync::NextBar,
        crossfade_secs: 0.0,
        segment: Some(MusicSegment {
            asset: level(0.3, BEAT),
            volume_db: 0.0,
        }),
    });
    audio.play_music(&combat, Duration::ZERO).expect("plays");
    let _ = render(&mut audio, BAR / 2, 800);
    audio
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("schedules");
    // Into the segment: the switch has happened, so this cannot be taken back any more.
    let _ = render(&mut audio, BAR / 2 + BEAT / 2, 800);
    assert_eq!(audio.current_music(), Some("combat"));
    audio
        .music_transition("menu", MusicTransitionRequest::default())
        .expect("queues");
    // Over the rest of the segment and into calm: the queued request schedules on calm's grid.
    let _ = render(&mut audio, BEAT, 800);
    assert_eq!(audio.current_music(), Some("calm"));
    assert_eq!(audio.music_state().pending.map(|p| p.to).as_deref(), Some("menu"));
    let heard = render(&mut audio, 2 * BAR, 800);
    assert_eq!(audio.current_music(), Some("menu"));
    assert_near(*heard.last().expect("samples"), 0.6, "the queued track");
}

#[test]
fn a_transition_with_nothing_playing_starts_the_track() {
    let mut audio = headless();
    audio.register_music(track("calm", 0.4));
    let schedule = audio
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("starts");
    assert_eq!(schedule.sync, MusicSync::Immediate);
    assert_eq!(audio.current_music(), Some("calm"));
    let heard = render(&mut audio, 800, 800);
    assert_near(heard[0], 0.4, "from the first sample");
}

#[test]
fn an_unregistered_track_is_an_error_and_changes_nothing() {
    let mut audio = headless();
    audio
        .play_music(&track("combat", 0.2), Duration::ZERO)
        .expect("plays");
    let error = audio
        .music_transition("nowhere", MusicTransitionRequest::default())
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        error.as_deref(),
        Some("audio cue or track `nowhere` is not registered")
    );
    assert_eq!(audio.current_music(), Some("combat"));
}

#[test]
fn play_music_replaces_a_transition_on_its_way() {
    let mut audio = headless();
    audio.register_music(track("calm", 0.4));
    audio
        .play_music(&track("combat", 0.2), Duration::ZERO)
        .expect("plays");
    let _ = render(&mut audio, BEAT, 800);
    audio
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("schedules");
    audio
        .play_music(&track("menu", 0.6), Duration::ZERO)
        .expect("plays");
    assert_eq!(audio.music_state().pending, None);
    let heard = render(&mut audio, 2 * BAR, 800);
    assert_eq!(audio.current_music(), Some("menu"));
    assert_near(*heard.last().expect("samples"), 0.6, "only menu remains");
}

#[test]
fn a_suspend_drops_what_was_on_its_way() {
    let mut audio = headless();
    audio.register_music(track("calm", 0.4));
    let mut asset = track("combat", 0.2);
    asset.stingers.push(stinger("hit", 0.5, MusicSync::NextBar));
    audio.play_music(&asset, Duration::ZERO).expect("plays");
    let _ = render(&mut audio, BEAT, 800);
    audio
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("schedules");
    audio.music_stinger("hit", None).expect("schedules");
    audio.suspend_output();
    let state = audio.music_state();
    assert_eq!(state.pending, None);
    assert!(state.stingers_playing.is_empty());
    assert_eq!(state.clock_secs, None);
    assert!(audio.resume_output());
    // A resumed game moves on: nothing to sync to, so the track simply starts.
    let schedule = audio
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("starts");
    assert_eq!(schedule.sync, MusicSync::Immediate);
    assert_eq!(audio.current_music(), Some("calm"));
}

#[test]
fn the_stems_of_a_track_start_on_the_same_sample_and_loop_on_the_grid() {
    // Two stems at their own levels: from the first sample the output is their sum.
    let mut audio = headless();
    let asset = MusicAsset::from_stems("t", 120.0, 4, vec![stem("a", 0.1), stem("b", 0.2)]);
    audio.play_music(&asset, Duration::ZERO).expect("plays");
    let heard = render(&mut audio, 2 * BAR, 800);
    assert_near(heard[0], 0.3, "both on the first sample");
    assert_near(heard[BAR], 0.3, "across the loop point");
}

// -- Beats to gameplay ------------------------------------------------------------------------

/// Collects every [`MusicBeat`] in `Update`, like gameplay would.
#[derive(Resource, Default)]
struct Heard(Vec<MusicBeat>);

fn listen(mut reader: MessageReader<MusicBeat>, mut heard: ResMut<Heard>) {
    heard.0.extend(reader.read().cloned());
}

fn beat_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    app.insert_resource(settings());
    app.add_plugins(AudioPlugin);
    app.init_resource::<Heard>();
    app.add_systems(regia_app::Update, listen);
    app
}

fn run_frames(app: &mut App, frames: usize, step: usize) {
    for _ in 0..frames {
        app.world_mut()
            .resource_mut::<Audio>()
            .render_offline(step)
            .expect("the offline backend");
        app.update();
    }
}

#[test]
fn gameplay_hears_every_beat_with_its_bar_and_beat_index() {
    let mut app = beat_app();
    app.update();
    app.world_mut()
        .resource_mut::<Audio>()
        .play_music(&track("t", 0.0), Duration::ZERO)
        .expect("plays");
    // 5 s at 120 bpm = 10 beats, at 800 frames a frame.
    run_frames(&mut app, 300, 800);
    let heard = &app.world().resource::<Heard>().0;
    assert!(heard.len() >= 9, "{} beats", heard.len());
    for (index, beat) in heard.iter().enumerate() {
        assert_eq!(beat.track, "t");
        assert_eq!(beat.beat_index, index as u64);
        assert_eq!(beat.bar, index as u64 / 4);
        assert_eq!(beat.beat, (index % 4) as u32);
        assert!((beat.clock_secs - index as f64 * 0.5).abs() < 1e-9);
    }
}

#[test]
fn a_long_frame_loses_no_beats() {
    let mut app = beat_app();
    app.update();
    app.world_mut()
        .resource_mut::<Audio>()
        .play_music(&track("t", 0.0), Duration::ZERO)
        .expect("plays");
    run_frames(&mut app, 2, 800);
    // A two-second stall: four beats arrive at once, in order.
    run_frames(&mut app, 2, 96_000);
    run_frames(&mut app, 1, 800);
    let indices: Vec<u64> = app
        .world()
        .resource::<Heard>()
        .0
        .iter()
        .map(|b| b.beat_index)
        .collect();
    assert!(indices.len() >= 4, "{indices:?}");
    assert!(indices.iter().copied().eq(0..indices.len() as u64), "{indices:?}");
}

#[test]
fn beats_follow_the_new_track_after_a_transition() {
    let mut app = beat_app();
    app.update();
    {
        let mut audio = app.world_mut().resource_mut::<Audio>();
        audio.register_music(track("calm", 0.0));
        audio
            .play_music(&track("combat", 0.0), Duration::ZERO)
            .expect("plays");
    }
    run_frames(&mut app, 30, 800);
    app.world_mut()
        .resource_mut::<Audio>()
        .music_transition("calm", MusicTransitionRequest::default())
        .expect("schedules");
    run_frames(&mut app, 240, 800);
    let heard = &app.world().resource::<Heard>().0;
    let combat: Vec<&MusicBeat> = heard.iter().filter(|b| b.track == "combat").collect();
    let calm: Vec<&MusicBeat> = heard.iter().filter(|b| b.track == "calm").collect();
    // The switch is at 2 s, on the fifth beat: combat's beats 0..=3, then calm's from its own 0.
    assert_eq!(combat.len(), 4, "{heard:?}");
    assert_eq!(
        calm.first().map(|b| (b.beat_index, b.bar, b.beat)),
        Some((0, 0, 0))
    );
    assert!((calm[0].clock_secs - 2.0).abs() < 1e-9);
    assert!(calm.len() >= 4, "{heard:?}");
}

#[test]
fn nothing_is_announced_before_the_mixer_has_run() {
    let mut app = beat_app();
    app.update();
    app.world_mut()
        .resource_mut::<Audio>()
        .play_music(&track("t", 0.0), Duration::ZERO)
        .expect("plays");
    // Frames with no rendering, as on a stalled output: the clock does not move.
    for _ in 0..30 {
        app.update();
    }
    assert!(app.world().resource::<Heard>().0.is_empty());
}
