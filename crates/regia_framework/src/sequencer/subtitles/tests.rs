// SPDX-License-Identifier: MPL-2.0
//! Acceptance tests of the sequence subtitles: fixed 250 ms steps, no clocks, no goldens.

use std::time::Duration;

use regia_app::caption::{CaptionStart, CaptionStop};
use regia_asset::dialogue::{parse_dialogue, parse_subtitles, parse_voice};
use regia_ui::captions::{CaptionClock, SubtitleSets};
use regia_ui::output::{ForcedOutput, UiLayoutVariant, UiOutput};

use super::super::asset::fixtures::{marker, sequence};
use super::super::asset::{AudioSection, Channel, SubRepeat, SubSection, Track};
use super::super::player::{EndPolicy, SequencerPlugin};
use super::super::sample::SequenceLibrary;
use super::*;

const STEP_US: u64 = 250_000;

const DIALOGUE: &str = r#"(
    format_version: 1,
    name: "intro",
    lines: [
        (id: "hello", speaker: "ranger",
         variants: [(sound: "audio/hello.ogg", cues: Some([
            (at: 0.0, until: Some(1.0), text_key: Some("cue-one")),
            (at: 1.0, until: Some(2.0), text: Some("[raw two]")),
            (at: 2.0, until: Some(3.0), text_key: Some("cue-three")),
         ]))]),
        (id: "side", speaker: "ranger", variants: [(sound: "audio/side.ogg")]),
    ],
)"#;
const EN: &str = "cue-one = One\ncue-three = Three\nvoice-ranger-name = Ranger\n";
const FR: &str = "cue-one = Un\ncue-three = Trois\n";

fn library() -> DialogueLibrary {
    let mut lib = DialogueLibrary::default();
    let dialogue = parse_dialogue("intro.dialogue.ron", DIALOGUE).expect("dialogue");
    lib.dialogues.insert(dialogue.name.clone(), dialogue);
    let voice = parse_voice(
        "ranger.voice.ron",
        r#"(format_version: 1, id: "ranger", name_key: "voice-ranger-name")"#,
    )
    .expect("voice");
    lib.voices.insert(voice.id.clone(), voice);
    let subs = parse_subtitles(
        "audio/side.subs.ron",
        r#"(format_version: 1, cues: [(at: 0.0, until: Some(1.0), text: Some("side"))])"#,
    )
    .expect("subs");
    lib.subtitles.insert("audio/side.subs.ron".to_owned(), subs);
    lib
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SequencerPlugin))
        .insert_resource(bevy_time_step())
        .insert_resource(library())
        .insert_resource(CaptionClock(0))
        .insert_resource(
            Localization::new(
                vec![("en".to_owned(), EN.to_owned()), ("fr".to_owned(), FR.to_owned())],
                Some("en".to_owned()),
            )
            .expect("locales"),
        );
    app
}

fn bevy_time_step() -> TimeUpdateStrategy {
    TimeUpdateStrategy::ManualDuration(Duration::from_micros(STEP_US))
}

fn audio_track(name: &str, section: AudioSection) -> Track {
    Track {
        name: name.into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Audio(vec![section]),
        enabled: true,
        locked: false,
    }
}

fn spawn(app: &mut App, tracks: Vec<Track>, end: f32) -> Entity {
    let mut player = SequencePlayer::new(sequence(tracks, vec![marker("end", end)]));
    player.end = EndPolicy::Hold;
    player.play();
    app.world_mut().spawn(player).id()
}

fn step(app: &mut App) {
    app.world_mut().resource_mut::<CaptionClock>().0 += STEP_US;
    app.update();
}

fn texts(app: &App, player: Entity) -> Vec<String> {
    app.world()
        .resource::<SequenceSubtitles>()
        .active(player)
        .into_iter()
        .map(|a| a.text)
        .collect()
}

fn line_section() -> AudioSection {
    AudioSection::new("hello", 0.0, 3.0)
}

#[test]
fn a_section_playing_a_known_line_shows_its_cues_at_the_right_times() {
    let mut app = app();
    let p = spawn(&mut app, vec![audio_track("voice", line_section())], 4.0);
    let mut seen = Vec::new();
    for _ in 0..12 {
        step(&mut app);
        seen.push(texts(&app, p).first().cloned().unwrap_or_default());
    }
    // Updates land on t = 0.25, 0.5, ... 3.0 (the clock steps before the sample).
    let expect = |t: f64| match t {
        t if t < 1.0 => "One",
        t if t < 2.0 => "[raw two]",
        t if t < 3.0 => "Three",
        _ => "",
    };
    for (i, got) in seen.iter().enumerate() {
        let t = 0.25 * (i + 1) as f64;
        assert_eq!(got, expect(t), "at t = {t}");
    }
    let active = app.world().resource::<SequenceSubtitles>().active(p);
    assert!(active.is_empty(), "past the section nothing shows");
    assert_eq!(app.world().resource::<SequenceSubtitles>().requested(), 3);
}

#[test]
fn the_speaker_and_the_set_reach_the_caption_board() {
    let mut app = app();
    let p = spawn(&mut app, vec![audio_track("voice", line_section())], 4.0);
    step(&mut app);
    let active = app.world().resource::<SequenceSubtitles>().active(p);
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].speaker, "ranger");
    assert_eq!(
        (active[0].dialogue.as_str(), active[0].line.as_str()),
        ("intro", "hello")
    );
    assert!(active[0].caption.is_some());
    assert!(app.world().resource::<SubtitleSets>().is_active());
    let starts = app.world().resource::<Messages<CaptionStart>>();
    let mut cursor = starts.get_cursor();
    let sent: Vec<&CaptionStart> = cursor.read(starts).collect();
    // The set is sent again once when it re-bases onto the playhead (same id).
    assert!(!sent.is_empty());
    assert!(sent.iter().all(|c| Some(c.id) == active[0].caption));
    assert!(sent.iter().all(|c| c.speaker.as_deref() == Some("ranger")));
}

#[test]
fn a_seek_into_the_middle_shows_the_right_cue() {
    let mut app = app();
    let p = spawn(&mut app, vec![audio_track("voice", line_section())], 4.0);
    step(&mut app);
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .seek(1.5);
    step(&mut app); // t = 1.75
    assert_eq!(texts(&app, p), ["[raw two]"]);
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .seek(2.25);
    step(&mut app); // t = 2.5
    assert_eq!(texts(&app, p), ["Three"]);
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .seek(0.25);
    step(&mut app); // t = 0.5
    assert_eq!(texts(&app, p), ["One"], "a scrub back");
    let stops = app.world().resource::<Messages<CaptionStop>>().len();
    assert!(stops >= 2, "each cue change stopped the set before it");
}

#[test]
fn a_time_scale_of_one_half_stretches_the_cue_times() {
    let mut app = app();
    let mut slow = line_section();
    slow.end = 6.0;
    slow.time_scale = 0.5;
    let p = spawn(&mut app, vec![audio_track("voice", slow)], 7.0);
    let mut seen = Vec::new();
    for _ in 0..8 {
        step(&mut app);
        seen.push(texts(&app, p).first().cloned().unwrap_or_default());
    }
    // t = 0.25 .. 2.0: sound position is t / 2, so cue one (0..1 s of sound) lasts until t = 2.
    assert_eq!(seen[..7], ["One"; 7], "{seen:?}");
    assert_eq!(seen[7], "[raw two]", "{seen:?}");
}

#[test]
fn a_start_offset_shifts_the_cue() {
    let mut app = app();
    let mut shifted = AudioSection::new("hello", 1.0, 3.0);
    shifted.offset = 1.0;
    let p = spawn(&mut app, vec![audio_track("voice", shifted)], 4.0);
    for _ in 0..3 {
        step(&mut app);
    }
    assert!(texts(&app, p).is_empty(), "t = 0.75 is before the section");
    step(&mut app); // t = 1.0: sound position 1.0
    assert_eq!(texts(&app, p), ["[raw two]"]);
}

#[test]
fn a_pause_holds_the_cue_and_play_goes_on() {
    let mut app = app();
    let p = spawn(&mut app, vec![audio_track("voice", line_section())], 4.0);
    for _ in 0..5 {
        step(&mut app); // t = 1.25
    }
    assert_eq!(texts(&app, p), ["[raw two]"]);
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .pause();
    for _ in 0..12 {
        step(&mut app); // three seconds of caption clock pass
        assert_eq!(texts(&app, p), ["[raw two]"], "held");
        assert!(
            app.world().resource::<SubtitleSets>().is_active(),
            "the set is still live"
        );
    }
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .play();
    for _ in 0..3 {
        step(&mut app); // t = 2.0
    }
    assert_eq!(texts(&app, p), ["Three"]);
}

#[test]
fn a_looping_sub_sequence_repeats_its_cues() {
    let mut app = app();
    let inner = sequence(
        vec![audio_track("voice", line_section())],
        vec![marker("end", 3.0)],
    );
    let mut library = SequenceLibrary::default();
    library.insert("inner", inner);
    app.world_mut().insert_resource(library);
    let mut sub = SubSection::new("inner", 0.0, 12.0);
    sub.repeat = SubRepeat::UntilStop;
    let outer = Track {
        name: "fx".into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Sub(vec![sub]),
        enabled: true,
        locked: false,
    };
    let p = spawn(&mut app, vec![outer], 12.0);
    let mut first_cues = 0;
    let mut last = String::new();
    for _ in 0..24 {
        step(&mut app);
        let now = texts(&app, p).first().cloned().unwrap_or_default();
        if now == "One" && last != "One" {
            first_cues += 1;
        }
        last = now;
    }
    assert!(
        first_cues >= 2,
        "the first cue came back with each pass: {first_cues}"
    );
}

#[test]
fn a_stop_or_a_finish_or_a_despawn_clears_the_cues() {
    let mut app = app();
    let p = spawn(&mut app, vec![audio_track("voice", line_section())], 4.0);
    step(&mut app);
    assert!(app.world().resource::<SubtitleSets>().is_active());
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .stop();
    step(&mut app);
    assert!(texts(&app, p).is_empty());
    assert!(
        !app.world().resource::<SubtitleSets>().is_active(),
        "skip clears the set"
    );
    // A despawn.
    let q = spawn(&mut app, vec![audio_track("voice", line_section())], 4.0);
    step(&mut app);
    assert!(app.world().resource::<SubtitleSets>().is_active());
    app.world_mut().despawn(q);
    step(&mut app);
    assert!(!app.world().resource::<SubtitleSets>().is_active());
    assert!(app.world().resource::<SequenceSubtitles>().is_empty());
}

#[test]
fn a_locale_switch_swaps_the_text_at_the_next_cue() {
    let mut app = app();
    let p = spawn(&mut app, vec![audio_track("voice", line_section())], 4.0);
    step(&mut app); // t = 0.25
    assert_eq!(texts(&app, p), ["One"]);
    assert!(app.world_mut().resource_mut::<Localization>().set_locale("fr"));
    step(&mut app); // t = 0.5: the running cue keeps its text
    assert_eq!(texts(&app, p), ["One"]);
    for _ in 0..6 {
        step(&mut app); // t = 2.0: cue three
    }
    let active = app.world().resource::<SequenceSubtitles>().active(p);
    assert_eq!(active[0].text, "Trois");
    assert_eq!(active[0].locale, "fr");
}

#[test]
fn an_opted_out_section_shows_none() {
    let mut app = app();
    let a = spawn(&mut app, vec![audio_track("voice.nosubs", line_section())], 4.0);
    let b = spawn(&mut app, vec![audio_track("voice", line_section())], 4.0);
    app.world_mut()
        .resource_mut::<SequenceSubtitleSettings>()
        .opt_out_tracks
        .insert("voice".to_owned());
    for _ in 0..2 {
        step(&mut app);
    }
    assert!(texts(&app, a).is_empty(), "the suffix opts a track out");
    assert!(texts(&app, b).is_empty(), "the settings opt a track out");
    app.world_mut()
        .resource_mut::<SequenceSubtitleSettings>()
        .opt_out_tracks
        .clear();
    step(&mut app);
    assert!(texts(&app, a).is_empty());
    assert_eq!(texts(&app, b), ["One"], "removing the opt-out shows the cue");
}

#[test]
fn a_sound_with_no_line_shows_nothing_and_a_sidecar_line_shows_its_cues() {
    let mut app = app();
    let stranger = spawn(
        &mut app,
        vec![audio_track("voice", AudioSection::new("music", 0.0, 3.0))],
        4.0,
    );
    let side = spawn(
        &mut app,
        vec![audio_track(
            "voice",
            AudioSection::new("audio/side.ogg", 0.0, 3.0),
        )],
        4.0,
    );
    step(&mut app);
    assert!(texts(&app, stranger).is_empty());
    assert_eq!(texts(&app, side), ["side"]);
}

#[test]
fn render_out_requests_the_cues_only_when_the_job_asks() {
    let mut app = app();
    app.world_mut().init_resource::<UiOutput>();
    app.world_mut().resource_mut::<UiOutput>().forced = Some(ForcedOutput {
        size: [1920.0, 1080.0],
        variant: UiLayoutVariant::Landscape,
        pixel_scale: 1.0,
    });
    let p = spawn(&mut app, vec![audio_track("voice", line_section())], 4.0);
    for _ in 0..3 {
        step(&mut app);
    }
    assert!(texts(&app, p).is_empty(), "off by default in a render-out");
    app.world_mut().resource_mut::<SequenceSubtitleSettings>().burn_in = true;
    // The render job sets each frame's time; the cue is the one at that time.
    for (time, want) in [(0.5, "One"), (1.5, "[raw two]"), (2.5, "Three"), (0.75, "One")] {
        app.world_mut()
            .get_mut::<SequencePlayer>(p)
            .expect("player")
            .seek(time - 0.25);
        step(&mut app);
        assert_eq!(texts(&app, p), [want], "at {time}");
    }
}

#[test]
fn an_overlapping_later_cue_is_the_one_on_screen() {
    use regia_asset::dialogue::{TimedCue, cue_at};

    // spec 36 allows a later `at` while an earlier `until` is still open. `cue_at` shows the
    // later cue; the sequencer must show that same cue.
    let cues = vec![
        TimedCue {
            start_us: 0,
            end_us: 10_000_000,
            text_key: Some("outer".to_string()),
            text: None,
        },
        TimedCue {
            start_us: 2_000_000,
            end_us: 4_000_000,
            text_key: Some("inner".to_string()),
            text: None,
        },
    ];
    let at = 3_000_000;
    let spec = cue_at(&cues, at).and_then(|c| c.text_key.clone());
    let got = active_cue(&cues, at).map(|i| cues[i].text_key.clone().unwrap_or_default());
    assert_eq!(got.as_deref(), spec.as_deref());
    assert_eq!(got.as_deref(), Some("inner"));
}

#[test]
fn renaming_a_silent_section_to_a_voiced_line_shows_its_cues() {
    // The section at the same path first plays a sound with no line, so no caption set is live;
    // after the edit it plays a voiced line and must not keep the stale "no line" entry.
    let mut app = app();
    let tracks = |section| vec![audio_track("voice", section)];
    let p = spawn(&mut app, tracks(AudioSection::new("music", 0.0, 3.0)), 4.0);
    step(&mut app);
    assert!(texts(&app, p).is_empty());
    let sequence = sequence(tracks(line_section()), vec![marker("end", 4.0)]);
    app.world_mut()
        .get_mut::<SequencePlayer>(p)
        .expect("player")
        .replace_sequence(sequence);
    step(&mut app);
    assert_eq!(texts(&app, p), ["One"]);
}
