// SPDX-License-Identifier: MPL-2.0
//! Runner and plugin tests (spec 56 § 8 items 4, 13 (runner part), 14).

use std::collections::BTreeMap;

use regia_app::caption::{CaptionIds, CaptionTime};

use super::*;

struct Env {
    library: ConversationLibrary,
    texts: BTreeMap<String, String>,
    voice: FrameVoicePlayer,
}

impl RunnerEnv for Env {
    fn conversation(&self, id: &str) -> Option<&ConversationAsset> {
        self.library.conversations.get(id)
    }
    fn settings(&self) -> &ConversationSettings {
        &self.library.settings
    }
    fn speakers(&self) -> &SpeakerSet {
        &self.library.speakers
    }
    fn locale_loaded(&self, locale: &str) -> bool {
        locale == "en" || locale == "de"
    }
    fn active_locale(&self) -> String {
        "en".into()
    }
    fn chain(&self, locale: &str) -> Vec<String> {
        if locale == "en" {
            vec!["en".into()]
        } else {
            vec![locale.into(), "en".into()]
        }
    }
    fn text(&self, _locale: &str, key: &str) -> Option<String> {
        self.texts.get(key).cloned()
    }
    fn clip_exists(&self, path: &str) -> bool {
        self.library.clips.contains_key(path)
    }
    fn clip_duration_us(&self, path: &str) -> Option<u64> {
        self.library.clips.get(path).copied().flatten()
    }
    fn play_voice(&mut self, clip: &str) -> Option<VoiceStart> {
        let duration = self.library.clips.get(clip).copied().flatten();
        self.voice.play(clip, duration)
    }
    fn voice_playing(&self, handle: u64) -> bool {
        self.voice.playing(handle)
    }
}

fn line(id: &str) -> ConversationLine {
    ConversationLine {
        id: id.into(),
        speaker: "guard".into(),
        text: format!("k-{id}"),
        note: String::new(),
        emotion: None,
        tags: Vec::new(),
        voice: VoiceSpec::None,
        duration: DurationSource::Fixed(2000),
        cues: Vec::new(),
        priority: None,
        keep_breaks: false,
    }
}

fn env_with(steps: Vec<Step>, lines: &[&str]) -> Env {
    let mut library = ConversationLibrary::default();
    library
        .speakers
        .speakers
        .insert("guard".into(), Speaker::default());
    library.conversations.insert(
        "c".into(),
        ConversationAsset {
            format_version: 1,
            id: "c".into(),
            notes: String::new(),
            lines: lines.iter().map(|l| line(l)).collect(),
            steps,
        },
    );
    let mut texts = BTreeMap::new();
    for l in lines {
        texts.insert(format!("k-{l}"), format!("text of {l}"));
    }
    texts.insert("opt-a".into(), "Option A".into());
    texts.insert("opt-b".into(), "Option B".into());
    Env {
        library,
        texts,
        voice: FrameVoicePlayer::new(48_000),
    }
}

fn captions(outputs: &[RunnerOutput]) -> Vec<String> {
    outputs
        .iter()
        .filter_map(|o| match o {
            RunnerOutput::Caption(c) => Some(c.source.describe()),
            _ => None,
        })
        .collect()
}

fn finished(outputs: &[RunnerOutput]) -> Vec<FinishReason> {
    outputs
        .iter()
        .filter_map(|o| match o {
            RunnerOutput::Finished { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect()
}

#[test]
fn if_takes_then_for_true_and_else_for_false_and_unknown() {
    let steps = vec![
        Step::If {
            cond: "met".into(),
            then: "yes".into(),
            else_: "no".into(),
        },
        Step::Label("yes".into()),
        Step::Line("a".into()),
        Step::End,
        Step::Label("no".into()),
        Step::Line("b".into()),
        Step::End,
    ];
    for (flag, expected) in [(true, "c#a"), (false, "c#b")] {
        let mut env = env_with(steps.clone(), &["a", "b"]);
        let mut runner = ConversationRunner::default();
        runner.flags.set("met", flag);
        let mut ids = CaptionIds::default();
        runner.play(PlayRequest::new("c"), &env).expect("a value");
        let out = runner.tick(0, &mut env, &mut ids);
        assert_eq!(captions(&out), vec![format!("dialogue:1:{expected}")]);
    }
}

#[test]
fn the_first_some_in_the_chain_wins() {
    struct Fixed(bool);
    impl ConditionSource for Fixed {
        fn eval(&self, _name: &str) -> Option<bool> {
            Some(self.0)
        }
    }
    let steps = vec![
        Step::If {
            cond: "x".into(),
            then: "yes".into(),
            else_: "no".into(),
        },
        Step::Label("yes".into()),
        Step::Line("a".into()),
        Step::End,
        Step::Label("no".into()),
        Step::Line("b".into()),
        Step::End,
    ];
    let mut env = env_with(steps, &["a", "b"]);
    let mut runner = ConversationRunner::default();
    runner.add_source(Box::new(Fixed(true)));
    runner.add_source(Box::new(Fixed(false)));
    let mut ids = CaptionIds::default();
    runner.play(PlayRequest::new("c"), &env).expect("a value");
    let out = runner.tick(0, &mut env, &mut ids);
    assert_eq!(captions(&out), vec!["dialogue:1:c#a"]);
}

/// Spec 56 § 8 item 4: a source that answers an explicit `Some(false)` takes `else` (the flag
/// test above only covers an absent flag, i.e. "unknown"), and a `Some(false)` earlier in the chain
/// beats a later `Some(true)`.
#[test]
fn an_explicit_false_source_takes_else_and_wins_over_a_later_true() {
    struct Fixed(bool);
    impl ConditionSource for Fixed {
        fn eval(&self, _name: &str) -> Option<bool> {
            Some(self.0)
        }
    }
    let steps = vec![
        Step::If {
            cond: "x".into(),
            then: "yes".into(),
            else_: "no".into(),
        },
        Step::Label("yes".into()),
        Step::Line("a".into()),
        Step::End,
        Step::Label("no".into()),
        Step::Line("b".into()),
        Step::End,
    ];
    for sources in [vec![false], vec![false, true]] {
        let mut env = env_with(steps.clone(), &["a", "b"]);
        let mut runner = ConversationRunner::default();
        for value in &sources {
            runner.add_source(Box::new(Fixed(*value)));
        }
        let mut ids = CaptionIds::default();
        runner.play(PlayRequest::new("c"), &env).expect("a value");
        let out = runner.tick(0, &mut env, &mut ids);
        assert_eq!(captions(&out), vec!["dialogue:1:c#b"], "sources {sources:?}");
    }
}

#[test]
fn lines_run_in_order_on_their_planned_length() {
    let mut env = env_with(Vec::new(), &["a", "b"]);
    let mut runner = ConversationRunner::default();
    let mut ids = CaptionIds::default();
    runner.play(PlayRequest::new("c"), &env).expect("a value");
    assert_eq!(
        captions(&runner.tick(0, &mut env, &mut ids)),
        vec!["dialogue:1:c#a"]
    );
    assert!(runner.tick(1_999_999, &mut env, &mut ids).is_empty());
    let out = runner.tick(2_000_000, &mut env, &mut ids);
    assert_eq!(captions(&out), vec!["dialogue:1:c#b"]);
    let out = runner.tick(4_000_000, &mut env, &mut ids);
    assert_eq!(finished(&out), vec![FinishReason::Finished]);
    assert!(!runner.has_work());
}

#[test]
fn caption_carries_text_speaker_and_planned_time() {
    let mut env = env_with(Vec::new(), &["a"]);
    let mut runner = ConversationRunner::default();
    let mut ids = CaptionIds::default();
    runner.play(PlayRequest::new("c"), &env).expect("a value");
    let out = runner.tick(500, &mut env, &mut ids);
    let Some(RunnerOutput::Caption(c)) = out.first() else {
        panic!("no caption in {out:?}");
    };
    assert_eq!(c.speaker.as_deref(), Some("guard"));
    assert_eq!(c.planned_us, 2_000_000);
    assert_eq!(c.start, CaptionTime::AtMicros(500));
    assert_eq!(c.pieces.len(), 1);
}

#[test]
fn a_goto_loop_stops_at_the_step_budget() {
    let steps = vec![Step::Label("a".into()), Step::Goto("a".into())];
    let mut env = env_with(steps, &["x"]);
    let mut runner = ConversationRunner::default();
    let mut ids = CaptionIds::default();
    runner.play(PlayRequest::new("c"), &env).expect("a value");
    let out = runner.tick(0, &mut env, &mut ids);
    assert_eq!(finished(&out), vec![FinishReason::Error]);
}

#[test]
fn event_is_emitted_once_per_pass() {
    let steps = vec![Step::Event("door_open".into()), Step::Line("a".into()), Step::End];
    let mut env = env_with(steps, &["a"]);
    let mut runner = ConversationRunner::default();
    let mut ids = CaptionIds::default();
    runner.play(PlayRequest::new("c"), &env).expect("a value");
    let out = runner.tick(0, &mut env, &mut ids);
    let events: Vec<_> = out
        .iter()
        .filter(|o| matches!(o, RunnerOutput::Event { name, .. } if name == "door_open"))
        .collect();
    assert_eq!(events.len(), 1);
    let out = runner.tick(1, &mut env, &mut ids);
    assert!(out.iter().all(|o| !matches!(o, RunnerOutput::Event { .. })));
}

#[test]
fn choice_filters_by_condition_and_choose_jumps() {
    let steps = vec![
        Step::Choice {
            prompt: None,
            options: vec![
                ChoiceOption {
                    text: "opt-a".into(),
                    cond: Some("secret".into()),
                    goto: "a".into(),
                },
                ChoiceOption {
                    text: "opt-b".into(),
                    cond: None,
                    goto: "b".into(),
                },
            ],
        },
        Step::Label("a".into()),
        Step::Line("la".into()),
        Step::End,
        Step::Label("b".into()),
        Step::Line("lb".into()),
        Step::End,
    ];
    let mut env = env_with(steps, &["la", "lb"]);
    let mut runner = ConversationRunner::default();
    let mut ids = CaptionIds::default();
    let run = runner
        .play(PlayRequest::new("c"), &env)
        .expect("a value")
        .run
        .expect("a value");
    let out = runner.tick(0, &mut env, &mut ids);
    let Some(RunnerOutput::ChoiceRequested { options, .. }) = out.first() else {
        panic!("no choice in {out:?}");
    };
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].text, "Option B");
    assert_eq!(
        runner.choose(run, 5, &env),
        Err(ConversationError::ChoiceOutOfRange)
    );
    runner.choose(run, 0, &env).expect("a value");
    assert_eq!(
        runner.choose(run, 0, &env),
        Err(ConversationError::NoChoicePending)
    );
    let out = runner.tick(1, &mut env, &mut ids);
    assert_eq!(captions(&out), vec!["dialogue:1:c#lb"]);
}

#[test]
fn choice_with_no_available_option_takes_the_first_unconditional_or_ends() {
    let steps = vec![
        Step::Choice {
            prompt: None,
            options: vec![ChoiceOption {
                text: "opt-a".into(),
                cond: Some("never".into()),
                goto: "a".into(),
            }],
        },
        Step::Label("a".into()),
        Step::Line("la".into()),
    ];
    let mut env = env_with(steps, &["la"]);
    let mut runner = ConversationRunner::default();
    let mut ids = CaptionIds::default();
    runner.play(PlayRequest::new("c"), &env).expect("a value");
    assert_eq!(
        finished(&runner.tick(0, &mut env, &mut ids)),
        vec![FinishReason::Finished]
    );
}

#[test]
fn busy_channel_policies() {
    let env = env_with(Vec::new(), &["a"]);
    let mut runner = ConversationRunner::default();
    runner.play(PlayRequest::new("c"), &env).expect("a value");
    assert_eq!(
        runner.play(PlayRequest::new("c"), &env),
        Err(ConversationError::ChannelBusy("main".into()))
    );
    let mut replace = PlayRequest::new("c");
    replace.busy = BusyPolicy::Replace;
    let started = runner.play(replace, &env).expect("a value");
    assert_eq!(started.run, Some(RunId(2)));
    let mut queue = PlayRequest::new("c");
    queue.busy = BusyPolicy::Queue;
    assert_eq!(runner.play(queue, &env).expect("a value").run, None);
    let mut other = PlayRequest::new("c");
    other.channel = "ambient".into();
    assert!(runner.play(other, &env).expect("a value").run.is_some());
}

#[test]
fn play_errors() {
    let env = env_with(Vec::new(), &["a"]);
    let mut runner = ConversationRunner::default();
    assert_eq!(
        runner.play(PlayRequest::new("nope"), &env),
        Err(ConversationError::NotFound("nope".into()))
    );
    let mut req = PlayRequest::new("c");
    req.locale = Some("fr".into());
    assert_eq!(
        runner.play(req, &env),
        Err(ConversationError::LocaleNotLoaded("fr".into()))
    );
    let mut req = PlayRequest::new("c");
    req.from = Some("ghost".into());
    assert_eq!(
        runner.play(req, &env),
        Err(ConversationError::LabelNotFound("ghost".into()))
    );
    assert_eq!(ConversationError::ChannelBusy("x".into()).code(), "channel_busy");
}

#[test]
fn stop_ends_the_caption_and_the_run() {
    let mut env = env_with(Vec::new(), &["a"]);
    let mut runner = ConversationRunner::default();
    let mut ids = CaptionIds::default();
    let run = runner
        .play(PlayRequest::new("c"), &env)
        .expect("a value")
        .run
        .expect("a value");
    runner.tick(0, &mut env, &mut ids);
    runner.stop(run, 100);
    let out = runner.tick(10, &mut env, &mut ids);
    assert!(out.iter().any(|o| matches!(o, RunnerOutput::CaptionStop(_))));
    assert_eq!(finished(&out), vec![FinishReason::Stopped]);
}

#[test]
fn say_validates_and_builds_a_caption() {
    let mut env = env_with(Vec::new(), &["a"]);
    let mut runner = ConversationRunner::default();
    let mut ids = CaptionIds::default();
    let req = SayRequest {
        text: Some("Hello".into()),
        speaker: Some("guard".into()),
        ..SayRequest::default()
    };
    let c = runner.say(req, 100, &mut env, &mut ids).expect("a value");
    assert_eq!(c.planned_us, 1_000_000); // estimate floored at min display
    let long = SayRequest {
        text: Some("x".repeat(513)),
        ..SayRequest::default()
    };
    assert_eq!(
        runner.say(long, 0, &mut env, &mut ids),
        Err(ConversationError::TextTooLong)
    );
    let bad_speaker = SayRequest {
        text: Some("x".into()),
        speaker: Some("ghost".into()),
        ..SayRequest::default()
    };
    assert_eq!(
        runner.say(bad_speaker, 0, &mut env, &mut ids),
        Err(ConversationError::SpeakerNotFound("ghost".into()))
    );
    let missing = SayRequest {
        line: Some("c#zzz".into()),
        ..SayRequest::default()
    };
    assert!(matches!(
        runner.say(missing, 0, &mut env, &mut ids),
        Err(ConversationError::LineNotFound(_))
    ));
    assert!(matches!(
        runner.say(SayRequest::default(), 0, &mut env, &mut ids),
        Err(ConversationError::InvalidParams(_))
    ));
    let authored = SayRequest {
        line: Some("c#a".into()),
        ..SayRequest::default()
    };
    let c = runner.say(authored, 0, &mut env, &mut ids).expect("a value");
    assert_eq!(c.planned_us, 2_000_000);
}

#[test]
fn voiced_lines_start_on_the_audio_clock_and_end_with_the_clip() {
    // Audio coupling (spec 56 § 8 item 14): the caption start equals the audio frame clock at
    // play, and the line ends when the voice reaches its last frame (no wall clock).
    let mut env = env_with(Vec::new(), &["a", "b"]);
    if let Some(c) = env.library.conversations.get_mut("c") {
        c.lines[0].voice = VoiceSpec::Auto;
        c.lines[0].duration = DurationSource::Clip;
    }
    env.library.settings.voice_path = "vo/{locale}/{conversation}_{line}.ogg".into();
    env.library.clips.insert("vo/en/c_a.ogg".into(), Some(500_000)); // 24000 frames
    env.voice.frames = 4_800; // 100 ms in
    let mut runner = ConversationRunner::default();
    let mut ids = CaptionIds::default();
    runner.play(PlayRequest::new("c"), &env).expect("a value");
    let out = runner.tick(100_000, &mut env, &mut ids);
    let Some(RunnerOutput::Caption(c)) = out.first() else {
        panic!("no caption in {out:?}");
    };
    assert_eq!(c.start, CaptionTime::AtMicros(100_000));
    assert_eq!(c.planned_us, 500_000);
    // One frame before the last frame of the clip: still playing.
    env.voice.frames = 4_800 + 23_999;
    assert!(runner.tick(599_000, &mut env, &mut ids).is_empty());
    // The last frame reached: the voice is done, the caption stops, the next line starts.
    env.voice.frames = 4_800 + 24_000;
    let out = runner.tick(600_000, &mut env, &mut ids);
    assert!(out.iter().any(|o| matches!(o, RunnerOutput::CaptionStop(_))));
    assert_eq!(captions(&out), vec!["dialogue:1:c#b"]);
}

#[test]
fn voiced_line_ends_at_the_planned_end_whatever_the_mixer_reports() {
    // Audit G finding 1 (ADR-4756): a voiced line ends when the runner's clock reaches the planned
    // end, never when the mixer reports "stopped". One player reports stopped early, one reports
    // playing past the planned end; both must emit the next line at the same `now_us`.
    let clip = "vo/en/c_a.ogg";
    let mk = || {
        let steps = vec![Step::Line("a".into()), Step::Line("b".into()), Step::End];
        let mut env = env_with(steps, &["a", "b"]);
        if let Some(c) = env.library.conversations.get_mut("c") {
            c.lines[0].voice = VoiceSpec::Auto;
            c.lines[0].duration = DurationSource::Clip;
        }
        env.library.settings.voice_path = "vo/{locale}/{conversation}_{line}.ogg".into();
        env.library.clips.insert(clip.into(), Some(500_000));
        env
    };
    let run = |env: &mut Env, early_stop: bool| {
        let mut runner = ConversationRunner::default();
        let mut ids = CaptionIds::default();
        runner.play(PlayRequest::new("c"), env).expect("a value");
        // The line starts (both players identical so far).
        let first = runner.tick(0, env, &mut ids);
        assert_eq!(captions(&first), vec!["dialogue:1:c#a"]);
        if early_stop {
            // The mixer has already consumed the whole clip: "stopped" from the next tick on.
            env.voice.frames = 48_000;
        }
        let mut log = Vec::new();
        for now in [499_999_u64, 500_000, 500_001] {
            let out = runner.tick(now, env, &mut ids);
            log.push((now, captions(&out)));
        }
        log
    };

    let mut early = mk();
    let mut late = mk();

    let a = run(&mut early, true);
    let b = run(&mut late, false);
    assert_eq!(a, b, "both mixers must give the same step sequence");
    assert_eq!(
        a[1],
        (500_000, vec!["dialogue:1:c#b".to_string()]),
        "the planned end, not the mixer, ends the line"
    );
}

#[test]
fn voiced_line_ends_on_the_runner_clock_not_the_player_start_clock() {
    // Muse read-only review of fix-framework2-determinism (high finding 1): `ends_us` must be
    // `now_us + planned_us` on the runner clock. This player's clock is 10 s ahead of the runner
    // when the line starts (a live device ahead of the simulation in slow motion, or an offline
    // render ahead of fixed-step time): the line must still end at the planned runner time. The
    // caption board keeps the player's start as a presentation stamp.
    let clip = "vo/en/c_a.ogg";
    let steps = vec![Step::Line("a".into()), Step::Line("b".into()), Step::End];
    let mut env = env_with(steps, &["a", "b"]);
    if let Some(c) = env.library.conversations.get_mut("c") {
        c.lines[0].voice = VoiceSpec::Auto;
        c.lines[0].duration = DurationSource::Clip;
    }
    env.library.settings.voice_path = "vo/{locale}/{conversation}_{line}.ogg".into();
    env.library.clips.insert(clip.into(), Some(500_000));
    // The device clock reads 10 s when the line starts (the existing planned-end test starts at
    // 0 and only moves afterwards, which masked this).
    env.voice.frames = 480_000;
    let mut runner = ConversationRunner::default();
    let mut ids = CaptionIds::default();
    runner.play(PlayRequest::new("c"), &env).expect("a value");
    let first = runner.tick(0, &mut env, &mut ids);
    let Some(RunnerOutput::Caption(c)) = first.first() else {
        panic!("no caption in {first:?}");
    };
    assert_eq!(
        c.start,
        CaptionTime::AtMicros(10_000_000),
        "the caption keeps the player's start as its presentation stamp"
    );
    let mut log = Vec::new();
    for now in [499_999_u64, 500_000, 500_001] {
        let out = runner.tick(now, &mut env, &mut ids);
        log.push((now, captions(&out)));
    }
    assert!(
        log[0].1.is_empty() && log[2].1.is_empty(),
        "the line ends exactly on the planned runner step: {log:?}"
    );
    assert_eq!(
        log[1],
        (500_000, vec!["dialogue:1:c#b".to_string()]),
        "the runner clock, not the player's start clock, ends the line"
    );
}

#[test]
fn unvoiced_line_ends_on_the_same_fixed_step_with_and_without_a_device_clock() {
    use regia_app::prelude::*;
    use regia_ecs::ecs::message::{MessageCursor, Messages};

    // Audit G finding 2 (ADR-4756): the runner ticks on fixed-step simulation time; the audio
    // device clock drives caption presentation only. A device clock that jumps one second must not
    // move the fixed step an unvoiced line's event lands on.
    let build = |device: bool| {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            regia_ui::captions::CaptionPlugin,
            ConversationPlugin,
        ));
        enable_fixed_step(&mut app, 60.0);
        let env = env_with(
            vec![Step::Line("a".into()), Step::Event("door".into()), Step::End],
            &["a"],
        );
        *app.world_mut().resource_mut::<ConversationLibrary>() = env.library.clone();
        {
            let mut runner = app.world_mut().resource_mut::<ConversationRunner>();
            runner.play(PlayRequest::new("c"), &env).expect("play");
        }
        if device {
            app.insert_resource(regia_audio::Audio::new(&regia_audio::AudioSettings {
                headless: true,
                sample_rate: 48_000,
                ..Default::default()
            }));
        }
        app
    };

    let event_index = |app: &mut App, jump_at: Option<usize>| {
        let mut cursor = MessageCursor::<ConversationEvent>::default();
        let mut index = None;
        for i in 1..=200 {
            if let Some(at) = jump_at {
                // A 1 s jump, then the device keeps pace with the simulation (one step per update).
                let frames = if i == at { 48_000 } else { 800 };
                if i >= at {
                    app.world_mut()
                        .resource_mut::<regia_audio::Audio>()
                        .render_offline(frames)
                        .expect("offline render");
                }
            }
            app.update();
            let events: Vec<_> = cursor
                .read(app.world().resource::<Messages<ConversationEvent>>())
                .collect();
            if events.iter().any(|e| e.name == "door") && index.is_none() {
                index = Some(i);
            }
        }
        index.expect("the event fires")
    };

    let mut with_device = build(true);
    let mut without_device = build(false);
    let a = event_index(&mut with_device, Some(20));
    let b = event_index(&mut without_device, None);
    assert_eq!(
        a, b,
        "the device clock must not move the fixed step the event lands on"
    );
    assert!(b > 20, "the 2 s line ends on its own fixed steps, not the jump");
}

/// A mono 16-bit PCM WAV of silence.
fn write_wav(path: &std::path::Path, frames: u32, rate: u32) {
    let data = frames * 2;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    bytes.resize(bytes.len() + data as usize, 0);
    std::fs::write(path, bytes).expect("the test directory is writable");
}

#[test]
fn the_engine_voice_starts_on_the_sample_clock_and_at_its_position() {
    // Spec 56 § 8 item 14 through the real mixer: the offline backend counts what it rendered.
    let dir = regia_test_utils::unique_temp_dir("voice-clock");
    write_wav(&dir.join("line.wav"), 4_800, 48_000);
    let mut audio = regia_audio::Audio::new(&regia_audio::AudioSettings {
        headless: true,
        sample_rate: 48_000,
        ..regia_audio::AudioSettings::default()
    });
    assert_eq!(audio.sample_clock(), None, "the output has not run");
    audio.render_offline(4_800).expect("offline");
    let mut device = CaptionDeviceClock::default();
    device.device_us = audio.sample_clock().map(regia_audio::SampleClock::micros);
    let now = device.step(0, None, false);
    assert_eq!(now, 100_000);
    audio.render_offline(2_400).expect("offline");
    let mut player = AudioVoicePlayer {
        audio: &mut audio,
        assets_dir: &dir,
        vfs: None,
        clock_us: now,
        device,
    };
    let start = player.play("line.wav", None).expect("the clip plays");
    assert_eq!(
        start.start_us, 150_000,
        "the frame counter, not the frame's clock"
    );
    assert_eq!(start.duration_us, Some(100_000));
    assert!(player.playing(start.handle));
    let at = regia_core::math::Vec3::new(3.0, 0.0, 0.0);
    let spatial = player.play_at("line.wav", None, at).expect("the clip plays");
    assert_eq!(spatial.start_us, 150_000);
    player.stop(start.handle, 0);
    player.stop(spatial.handle, 0);
    let positions: Vec<_> = audio.played().iter().map(|p| p.position).collect();
    assert_eq!(positions, [None, Some(at)]);
}

fn cue_offsets(env: &mut Env) -> Vec<u64> {
    let mut runner = ConversationRunner::default();
    let mut ids = CaptionIds::default();
    let mut request = PlayRequest::new("c");
    request.locale = Some("de".into());
    runner.play(request, &*env).expect("a value");
    let out = runner.tick(0, env, &mut ids);
    let Some(RunnerOutput::Caption(c)) = out.first() else {
        panic!("no caption in {out:?}");
    };
    c.pieces.iter().map(|p| p.offset_us).collect()
}

#[test]
fn rescale_cues_scales_offsets_by_the_clip_length_ratio() {
    let mut env = env_with(Vec::new(), &["a"]);
    if let Some(c) = env.library.conversations.get_mut("c") {
        c.lines[0].voice = VoiceSpec::Auto;
        c.lines[0].duration = DurationSource::Clip;
        c.lines[0].cues = vec![SubCue(0, "k-a".into()), SubCue(1_000, "k-a".into())];
    }
    env.library.settings.voice_path = "vo/{locale}/{conversation}_{line}.ogg".into();
    env.library.clips.insert("vo/en/c_a.ogg".into(), Some(2_000_000));
    env.library.clips.insert("vo/de/c_a.ogg".into(), Some(3_000_000));
    assert_eq!(
        cue_offsets(&mut env),
        vec![0, 1_000_000],
        "authored offsets win by default"
    );
    env.library.settings.rescale_cues = true;
    assert_eq!(cue_offsets(&mut env), vec![0, 1_500_000]);
    // No source clip: nothing to scale against.
    env.library.clips.remove("vo/en/c_a.ogg");
    assert_eq!(cue_offsets(&mut env), vec![0, 1_000_000]);
}

#[test]
fn identical_inputs_give_identical_outputs() {
    let run = || {
        let steps = vec![
            Step::Line("a".into()),
            Step::Event("e".into()),
            Step::Line("b".into()),
        ];
        let mut env = env_with(steps, &["a", "b"]);
        let mut runner = ConversationRunner::default();
        let mut ids = CaptionIds::default();
        runner.play(PlayRequest::new("c"), &env).expect("a value");
        let mut all = Vec::new();
        for t in [0, 1_000_000, 2_000_000, 4_000_000, 6_000_000] {
            all.extend(runner.tick(t, &mut env, &mut ids));
        }
        format!("{all:?}")
    };
    assert_eq!(run(), run());
}

#[test]
fn plugin_runs_a_conversation_into_caption_messages() {
    use regia_app::prelude::*;
    use regia_ui::captions::{CaptionBoard, CaptionPlugin, EntryState};

    let mut app = App::new();
    app.add_plugins((CaptionPlugin, ConversationPlugin));
    let env = env_with(Vec::new(), &["a"]);
    *app.world_mut().resource_mut::<ConversationLibrary>() = env.library.clone();
    {
        let mut runner = app.world_mut().resource_mut::<ConversationRunner>();
        let library = env.library;
        // The world has no Localization, so the runner shows the key text.
        let probe = Env {
            library,
            texts: BTreeMap::new(),
            voice: FrameVoicePlayer::new(48_000),
        };
        runner.play(PlayRequest::new("c"), &probe).expect("a value");
    }
    app.update();
    let board = app.world().resource::<CaptionBoard>();
    let snapshot = board.snapshot();
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(
        board.state_of(regia_app::caption::CaptionId(1)),
        Some(EntryState::Visible)
    );
}

/// Spec 56 § 4.1: the project settings in `conversation.ron` (`overlap`, `max_visible`,
/// `min_display_ms`, `max_extend_ms`, `reading_cps` per script) reach the caption board. Before
/// the fix `caption_policy()` was read only by the validator, so the board kept its defaults.
#[test]
fn conversation_settings_reach_the_caption_board() {
    use regia_app::prelude::*;
    use regia_ui::captions::{CaptionBoard, CaptionPlugin, OverlapPolicy};

    let mut app = App::new();
    app.add_plugins((CaptionPlugin, ConversationPlugin));
    // A field the settings file does not own, set by the game: it must survive.
    app.world_mut()
        .resource_mut::<CaptionBoard>()
        .policy
        .bottom_offset = 96.0;
    {
        let mut library = app.world_mut().resource_mut::<ConversationLibrary>();
        library.settings.overlap = OverlapPolicy::Queue;
        library.settings.max_visible = 1;
        library.settings.min_display_ms = 1500;
        library.settings.max_extend_ms = 500;
        library.settings.reading_cps = 20;
        library.settings.reading_cps_cjk = 7;
    }
    app.update();
    let policy = app.world().resource::<CaptionBoard>().policy.clone();
    assert_eq!(policy.overlap, OverlapPolicy::Queue);
    assert_eq!(policy.max_visible, 1);
    assert_eq!(policy.min_display_us, 1_500_000);
    assert_eq!(policy.max_extend_us, 500_000);
    assert_eq!(policy.cps_alphabetic, 20);
    assert_eq!(policy.cps_cjk, 7);
    assert_eq!(policy.bottom_offset, 96.0, "a game-set field is kept");

    // A later change (a reload of conversation.ron) is applied too.
    app.world_mut()
        .resource_mut::<ConversationLibrary>()
        .settings
        .max_visible = 2;
    app.update();
    assert_eq!(app.world().resource::<CaptionBoard>().policy.max_visible, 2);
}

/// Settings whose six caption fields are all default never overwrite a policy the game set on the
/// board itself.
#[test]
fn default_conversation_settings_keep_a_game_set_board_policy() {
    use regia_app::prelude::*;
    use regia_ui::captions::{CaptionBoard, CaptionPlugin, OverlapPolicy};

    let mut app = App::new();
    app.add_plugins((CaptionPlugin, ConversationPlugin));
    {
        let mut board = app.world_mut().resource_mut::<CaptionBoard>();
        board.policy.overlap = OverlapPolicy::ReplaceSameSpeaker;
        board.policy.max_visible = 5;
    }
    // Touch the library without changing its caption fields (a voice path edit).
    app.world_mut()
        .resource_mut::<ConversationLibrary>()
        .settings
        .voice_path = "vo/{locale}/{line}.ogg".into();
    app.update();
    app.update();
    let policy = app.world().resource::<CaptionBoard>().policy.clone();
    assert_eq!(policy.overlap, OverlapPolicy::ReplaceSameSpeaker);
    assert_eq!(policy.max_visible, 5);
}

/// rev-kimi-14 finding 4 (ADR-5936): `conversation.ron` caption settings that changed while no
/// caption board existed reach a board inserted later, and again a board that is re-created
/// (removed and inserted). Before the fix the system returned early without a board and then
/// waited for the next library change, so the later board kept its defaults.
#[test]
fn conversation_settings_reach_a_caption_board_inserted_later() {
    use regia_app::prelude::*;
    use regia_ui::captions::{CaptionBoard, OverlapPolicy};

    // No `CaptionPlugin` yet: the board does not exist.
    let mut app = App::new();
    app.add_plugins(ConversationPlugin);
    {
        let mut library = app.world_mut().resource_mut::<ConversationLibrary>();
        library.settings.overlap = OverlapPolicy::Queue;
        library.settings.max_visible = 1;
        library.settings.reading_cps = 20;
    }
    app.update();
    app.update();
    assert!(app.world().get_resource::<CaptionBoard>().is_none());

    // The board arrives in a frame where the library did not change.
    app.world_mut().insert_resource(CaptionBoard::default());
    app.update();
    let policy = app.world().resource::<CaptionBoard>().policy.clone();
    assert_eq!(
        policy.overlap,
        OverlapPolicy::Queue,
        "a board inserted later gets the settings"
    );
    assert_eq!(policy.max_visible, 1);
    assert_eq!(policy.cps_alphabetic, 20);

    // Re-created: removed, then a fresh board inserted.
    app.world_mut().remove_resource::<CaptionBoard>();
    app.update();
    app.world_mut().insert_resource(CaptionBoard::default());
    app.update();
    let policy = app.world().resource::<CaptionBoard>().policy.clone();
    assert_eq!(
        policy.overlap,
        OverlapPolicy::Queue,
        "a re-created board gets them again"
    );
    assert_eq!(policy.max_visible, 1);
}

/// A board inserted later while `conversation.ron` has only default caption fields keeps the
/// policy the game gave it (the default-settings rule holds for a late board too).
#[test]
fn default_conversation_settings_keep_a_late_board_policy() {
    use regia_app::prelude::*;
    use regia_ui::captions::{CaptionBoard, OverlapPolicy};

    let mut app = App::new();
    app.add_plugins(ConversationPlugin);
    app.update();
    let mut board = CaptionBoard::default();
    board.policy.overlap = OverlapPolicy::ReplaceSameSpeaker;
    board.policy.max_visible = 5;
    app.world_mut().insert_resource(board);
    app.update();
    app.update();
    let policy = app.world().resource::<CaptionBoard>().policy.clone();
    assert_eq!(policy.overlap, OverlapPolicy::ReplaceSameSpeaker);
    assert_eq!(policy.max_visible, 5);
}

#[test]
fn a_cue_with_a_caption_sends_it_once_per_play() {
    use regia_app::prelude::*;
    use regia_audio::{Audio, AudioSettings, CueCaptionDef, CueCaptionKind, CueEntry, SoundAsset, SoundCue};
    use regia_ui::captions::{CaptionBoard, CaptionOptions, CaptionPlugin};

    let mut app = App::new();
    app.add_plugins((CaptionPlugin, ConversationPlugin));
    // Sound captions show only in caption mode.
    app.world_mut().resource_mut::<CaptionOptions>().caption_mode = true;
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..Default::default()
    });
    let clip = regia_audio::SoundClip::new(vec![0.1_f32; 48_000], 48_000).expect("clip");
    let mut cue = SoundCue::new(
        "door",
        vec![CueEntry {
            asset: SoundAsset::from_clip(&clip),
            weight: 1.0,
        }],
    );
    cue.caption = Some(CueCaptionDef {
        key: "cap-door".into(),
        kind: CueCaptionKind::Sound,
        priority: 3,
        planned_ms: Some(1500),
    });
    audio.register_cue(cue);
    audio.play_cue("door").expect("play");
    app.insert_resource(audio);
    app.update();
    let snapshot = app.world().resource::<CaptionBoard>().snapshot();
    assert_eq!(snapshot.entries.len(), 1, "{snapshot:?}");
    assert_eq!(snapshot.entries[0]["source"], "sound_cue:door");
    assert_eq!(snapshot.entries[0]["kind"], "sound");
    assert_eq!(snapshot.entries[0]["priority"], 3);
    // A second frame sends nothing more: one caption per play.
    app.update();
    assert_eq!(app.world().resource::<CaptionBoard>().snapshot().entries.len(), 1);
}

#[test]
fn the_runner_works_without_the_caption_presenter() {
    use regia_app::prelude::*;

    let mut app = App::new();
    app.add_plugins(ConversationPlugin);
    let env = env_with(Vec::new(), &["a"]);
    *app.world_mut().resource_mut::<ConversationLibrary>() = env.library.clone();
    app.world_mut()
        .resource_mut::<ConversationRunner>()
        .play(PlayRequest::new("c"), &env)
        .expect("play");
    // Writing the caption messages must not need the presenter's plugin.
    app.update();
    let runs = app.world().resource::<ConversationRunner>().state();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, "line");
}

#[test]
fn conversation_clock_keeps_the_fractional_microsecond_of_a_fixed_step() {
    use regia_app::prelude::*;

    // 1/60 s is 16_666_667 ns. Truncating each step to microseconds drops 667 ns, so 120
    // steps sum to 1_999_920 µs while the simulation clock, converted once, is 2_000_000 µs.
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, ConversationPlugin));
    enable_fixed_step(&mut app, 60.0);
    for _ in 0..120 {
        app.update();
    }
    let clock = app.world().resource::<ConversationClock>().0;
    let sim = u64::try_from(app.world().resource::<Time>().elapsed().as_micros()).unwrap_or(u64::MAX);
    assert_eq!(
        clock, sim,
        "summing truncated steps drifts off the simulation clock"
    );
}

#[test]
fn a_voice_sample_clock_ahead_of_the_runner_does_not_stretch_the_line() {
    // The caption start may follow the device sample counter. The runner's deadline is the
    // planned length on its own clock (ADR-4756): a device that is already 50 ms ahead must
    // not hold the next line for those 50 ms too.
    let clip = "vo/en/c_a.ogg";
    let mut env = env_with(
        vec![Step::Line("a".into()), Step::Line("b".into()), Step::End],
        &["a", "b"],
    );
    if let Some(c) = env.library.conversations.get_mut("c") {
        c.lines[0].voice = VoiceSpec::Auto;
        c.lines[0].duration = DurationSource::Clip;
    }
    env.library.settings.voice_path = "vo/{locale}/{conversation}_{line}.ogg".into();
    env.library.clips.insert(clip.into(), Some(500_000));
    env.voice.frames = 2_400; // 48 kHz → 50_000 µs
    let mut runner = ConversationRunner::default();
    let mut ids = CaptionIds::default();
    runner.play(PlayRequest::new("c"), &env).expect("play");
    let first = runner.tick(0, &mut env, &mut ids);
    assert_eq!(captions(&first), vec!["dialogue:1:c#a"]);
    let early = runner.tick(499_999, &mut env, &mut ids);
    assert!(
        captions(&early).is_empty(),
        "still inside the planned 500 ms: {early:?}"
    );
    let on_time = runner.tick(500_000, &mut env, &mut ids);
    assert_eq!(
        captions(&on_time),
        vec!["dialogue:1:c#b".to_string()],
        "the line ends at now + planned, not at the device stamp + planned"
    );
}

#[test]
fn the_asset_types_are_reflected() {
    use regia_app::prelude::*;

    let mut app = App::new();
    app.add_plugins(ConversationPlugin);
    let registry = app.world().resource::<AppTypeRegistry>().read();
    for name in ["ConversationAsset", "Speaker", "ConversationSettings"] {
        assert!(
            registry
                .iter()
                .any(|r| r.type_info().type_path_table().ident() == Some(name)),
            "{name} is not registered"
        );
    }
}

#[test]
fn framework2_perf_step_cache() {
    let mut steps = Vec::new();
    for i in 0..20 {
        steps.push(Step::Label(format!("l{i}")));
        steps.push(Step::Goto(format!("l{}", i + 1)));
    }
    steps.push(Step::Label("l20".into()));
    steps.push(Step::Line("a".into()));
    let mut env = env_with(steps, &["a"]);
    let mut runner = ConversationRunner::default();
    let mut ids = CaptionIds::default();
    runner.play(PlayRequest::new("c"), &env).expect("play");
    let before = super::asset::effective_steps_calls();
    let out = runner.tick(0, &mut env, &mut ids);
    let delta = super::asset::effective_steps_calls() - before;
    assert_eq!(delta, 1, "one tick rebuilt the step list {delta} times");
    assert_eq!(captions(&out), vec!["dialogue:1:c#a".to_string()]);
}

#[test]
fn framework2_perf_voice_debug() {
    use std::time::Duration;

    use regia_app::prelude::*;
    use regia_audio::{Audio, AudioSettings, CueCaptionDef, CueCaptionKind, CueEntry, SoundAsset, SoundCue};
    use regia_ui::captions::{CaptionBoard, CaptionOptions, CaptionPlugin};

    let mut app = App::new();
    app.add_plugins((CaptionPlugin, ConversationPlugin));
    app.world_mut().resource_mut::<CaptionOptions>().caption_mode = true;
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..Default::default()
    });
    let clip = regia_audio::SoundClip::new(vec![0.1_f32; 48_000], 48_000).expect("clip");
    let mut cue = SoundCue::new(
        "door",
        vec![CueEntry {
            asset: SoundAsset::from_clip(&clip),
            weight: 1.0,
        }],
    );
    cue.caption = Some(CueCaptionDef {
        key: "cap-door".into(),
        kind: CueCaptionKind::Sound,
        priority: 3,
        planned_ms: Some(1500),
    });
    audio.register_cue(cue);
    let id = audio.play_cue("door").expect("registered").expect("played");
    app.insert_resource(audio);
    let polls = super::voice_debug_polls();
    app.update();
    assert_eq!(app.world().resource::<CaptionBoard>().snapshot().entries.len(), 1);
    let polls_after = super::voice_debug_polls();
    assert_eq!(
        polls_after, polls,
        "the retain walk formatted playback state ({polls} -> {polls_after})"
    );
    app.update();
    let polls2 = super::voice_debug_polls();
    assert_eq!(
        polls2, polls,
        "the steady retain formatted playback state again ({polls} -> {polls2})"
    );
    assert_eq!(app.world().resource::<CaptionBoard>().snapshot().entries.len(), 1);
    let hold_before = app
        .world()
        .resource::<CaptionBoard>()
        .snapshot()
        .entries
        .first()
        .and_then(|entry| entry.get("hold_until_us"))
        .and_then(serde_json::Value::as_u64)
        .expect("caption hold");
    app.world_mut()
        .resource_mut::<Audio>()
        .stop_voice(id, Duration::ZERO);
    assert!(
        !app.world().resource::<Audio>().voice_playing(id),
        "a stopped voice is not playing"
    );
    app.update();
    // The board keeps a stopped row until its minimum display time, so the entry count stays 1.
    // `stop` pulls `hold_until_us` in; that is the cue-caption path seeing `voice_playing` go false.
    let hold_after = app
        .world()
        .resource::<CaptionBoard>()
        .snapshot()
        .entries
        .first()
        .and_then(|entry| entry.get("hold_until_us"))
        .and_then(serde_json::Value::as_u64)
        .expect("caption hold after stop");
    assert!(
        hold_after < hold_before,
        "stopping the voice pulls the caption hold in ({hold_before} -> {hold_after})"
    );
}

/// corr-framework-a F4: the runner ticks on `ConversationClock` (fixed-step time) while the
/// caption board reads `CaptionClock`, which follows the audio device and can be ahead of it (a
/// device that started early, a load hitch the fixed clock clamped, slow motion). An unvoiced
/// line's caption must be stamped on the caption clock so the board shows it; before the fix it
/// was stamped with the runner's clock, seconds in the past, and finished before it was shown.
#[test]
fn an_unvoiced_line_shows_when_the_caption_clock_is_ahead_of_the_runner() {
    use regia_app::prelude::*;
    use regia_ui::captions::{CaptionBoard, CaptionPlugin, EntryState};

    let mut app = App::new();
    app.add_plugins((MinimalPlugins, CaptionPlugin, ConversationPlugin));
    enable_fixed_step(&mut app, 60.0);
    app.insert_resource(regia_audio::Audio::new(&regia_audio::AudioSettings {
        headless: true,
        ..regia_audio::AudioSettings::default()
    }));
    app.update();
    // The presenter's clock is 30 s ahead of the fixed-step clock.
    app.world_mut().resource_mut::<CaptionClock>().0 = 30_000_000;
    let env = env_with(Vec::new(), &["a"]);
    *app.world_mut().resource_mut::<ConversationLibrary>() = env.library.clone();
    app.world_mut()
        .resource_mut::<ConversationRunner>()
        .play(PlayRequest::new("c"), &env)
        .expect("play");
    app.update();
    let board = app.world().resource::<CaptionBoard>();
    assert_eq!(
        board.state_of(regia_app::caption::CaptionId(1)),
        Some(EntryState::Visible),
        "{:?}",
        board.snapshot()
    );
}

/// corr-framework-a F5: a render-out job resets the simulation clock to zero
/// (`Time::set_clocks`). The conversation clock must keep running from where it was; before the
/// fix it stayed put until the new simulation time passed the old one, so no line ended.
#[test]
fn the_conversation_clock_keeps_running_after_the_simulation_clock_is_reset() {
    use regia_app::prelude::*;

    let mut app = App::new();
    app.add_plugins((MinimalPlugins, ConversationPlugin));
    enable_fixed_step(&mut app, 60.0);
    for _ in 0..120 {
        app.update();
    }
    let before = app.world().resource::<ConversationClock>().0;
    assert!(before >= 1_900_000, "{before}");
    app.world_mut()
        .resource_mut::<Time>()
        .set_clocks(std::time::Duration::ZERO, std::time::Duration::ZERO);
    for _ in 0..30 {
        app.update();
    }
    let after = app.world().resource::<ConversationClock>().0;
    assert!(
        after >= before + 400_000,
        "the clock ran on after the reset: {before} -> {after}"
    );
}

/// Spec 56 § 2.1: `Speaker.side` in `speakers.ron` reaches the caption board, so a speaker set to
/// `End` gets its label on the reading-end side of the row.
#[test]
fn the_speaker_side_reaches_the_caption_board() {
    use regia_app::caption::{
        CaptionId, CaptionKind, CaptionPiece, CaptionSource, CaptionStart, CaptionStyleHint,
    };
    use regia_app::prelude::*;
    use regia_ui::captions::{CaptionBoard, CaptionOptions, CaptionPlugin};

    let mut app = App::new();
    app.add_plugins((CaptionPlugin, ConversationPlugin));
    {
        let mut library = app.world_mut().resource_mut::<ConversationLibrary>();
        for (id, side) in [
            ("guard", asset::LabelSide::End),
            ("pilot", asset::LabelSide::Start),
        ] {
            library.speakers.speakers.insert(
                id.to_string(),
                Speaker {
                    side,
                    ..Speaker::default()
                },
            );
        }
    }
    app.update();
    for (n, speaker) in [(1, "guard"), (2, "pilot")] {
        app.world_mut().write_message(CaptionStart {
            id: CaptionId(n),
            source: CaptionSource::Game("test".into()),
            kind: CaptionKind::Speech,
            locale: "en".into(),
            pieces: vec![CaptionPiece::literal(0, "words")],
            speaker: Some(speaker.into()),
            priority: 0,
            start: CaptionTime::Now,
            planned_us: 5_000_000,
            keep_breaks: false,
            hint: CaptionStyleHint::default(),
        });
    }
    app.update();
    let opts = app.world().resource::<CaptionOptions>().clone();
    let rows = app.world().resource::<CaptionBoard>().visible_rows(&opts);
    let sides: Vec<_> = rows.iter().map(|r| (r.id, r.label_side)).collect();
    assert_eq!(
        sides,
        vec![
            (CaptionId(1), regia_ui::captions::LabelSide::End),
            (CaptionId(2), regia_ui::captions::LabelSide::Start)
        ]
    );
}
