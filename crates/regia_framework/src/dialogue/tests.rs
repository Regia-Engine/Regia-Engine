// SPDX-License-Identifier: MPL-2.0
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use regia_app::caption::{CaptionStart, CaptionStop, CaptionText};

use super::*;
use crate::conversation::{ConversationVoice, FrameVoicePlayer, VoiceStart};

const DIALOGUE: &str = r#"(
    format_version: 1,
    name: "ranger_intro",
    lines: [
        (id: "greeting", speaker: "ranger", listeners: ["player"], note: "At the gate.",
         display_key: Some("dlg-ranger_intro-greeting-short"),
         variants: [
            (sound: "audio/greeting.ogg"),
            (speaker: Some("ranger"), listener: Some("child"), sound: "audio/greeting_child.ogg"),
         ]),
        (id: "cued", speaker: "ranger",
         variants: [(sound: "audio/cued.ogg", cues: Some([
            (at: 0.0, until: Some(1.0), text_key: Some("cue-one")),
            (at: 1.5, text: Some("[laughs]")),
         ]))]),
        (id: "sidecar", speaker: "ranger", variants: [(sound: "audio/side.ogg")]),
        (id: "only_guard", speaker: "ranger", variants: [(speaker: Some("guard"), sound: "audio/g.ogg")]),
    ],
)"#;

const EN: &str = "dlg-ranger_intro-greeting = { $speaker } greets { $listener_gender ->\n    [feminine] her\n   *[other] their\n} friend.\ndlg-ranger_intro-greeting-short = Hello!\nvoice-ranger-name = Ranger\nvoice-player-name = Player\nvoice-child-name = Child\ncue-one = First cue\ndlg-ranger_intro-cued = Cued\ndlg-ranger_intro-sidecar = Side\ndlg-ranger_intro-only_guard = Guard\n";
const FR: &str = "dlg-ranger_intro-greeting-short = Bonjour !\ncue-one = Premier\n";

fn voice(id: &str, gender: &str) -> String {
    format!(r#"(format_version: 1, id: "{id}", name_key: "voice-{id}-name", gender: {gender})"#)
}

fn library() -> DialogueLibrary {
    let mut lib = DialogueLibrary::default();
    lib.insert_text(FileKind::Dialogue, "dialogue/ranger_intro.dialogue.ron", DIALOGUE)
        .expect("dialogue");
    for (id, gender) in [
        ("ranger", "Feminine"),
        ("player", "Feminine"),
        ("child", "Neutral"),
    ] {
        lib.insert_text(
            FileKind::Voice,
            &format!("voices/{id}.voice.ron"),
            &voice(id, gender),
        )
        .expect("voice");
    }
    lib.insert_text(
        FileKind::Subtitles,
        "audio/side.subs.ron",
        r#"(format_version: 1, priority: 2, cues: [(at: 0.0, text: Some("side cue"))])"#,
    )
    .expect("subs");
    for sound in [
        "audio/greeting.ogg",
        "localized/ja/audio/greeting.ogg",
        "audio/cued.ogg",
        "audio/side.ogg",
    ] {
        lib.sounds.insert(sound.to_owned());
    }
    lib
}

fn loc() -> Localization {
    Localization::new(
        vec![("en".to_owned(), EN.to_owned()), ("fr".to_owned(), FR.to_owned())],
        Some("en".to_owned()),
    )
    .expect("locales")
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(DialoguePlugin);
    app.insert_resource(library());
    app.insert_resource(loc());
    app.insert_resource(CaptionClock(0));
    app.insert_resource(ConversationVoice(Some(Box::new(FrameVoicePlayer::new(48_000)))));
    app
}

fn req(line: &str) -> DialogueRequest {
    DialogueRequest::new("ranger_intro", line)
}

#[test]
fn grammatical_arguments_reach_the_message_and_display_key_overrides_on_screen_only() {
    let lib = library();
    let l = loc();
    let r = resolve_line(&lib, Some(&l), &req("greeting")).expect("resolves");
    assert_eq!(r.text, "Ranger greets her friend.", "listener_gender feminine");
    assert_eq!(r.display_text, "Hello!");
    assert_eq!(r.variant, 0);
    assert!(
        r.args
            .contains(&("speaker_gender".to_owned(), "feminine".to_owned()))
    );
    assert!(
        r.args
            .contains(&("listener_number".to_owned(), "singular".to_owned()))
    );
    let to_child = DialogueRequest {
        listeners: Some(vec!["child".into()]),
        ..req("greeting")
    };
    let r = resolve_line(&lib, Some(&l), &to_child).expect("child");
    assert_eq!(r.variant, 1);
    assert_eq!(
        r.text, "Ranger greets their friend.",
        "neutral listener: the default"
    );
}

#[test]
fn unknown_asset_line_voice_and_no_variant_are_errors_with_codes() {
    let lib = library();
    let l = loc();
    let code = |r: DialogueRequest| {
        resolve_line(&lib, Some(&l), &r)
            .map(|_| ())
            .expect_err("an error")
            .code()
    };
    assert_eq!(
        code(DialogueRequest::new("nope", "greeting")),
        "dialogue_unknown_asset"
    );
    assert_eq!(code(req("nope")), "dialogue_unknown_line");
    assert_eq!(
        code(DialogueRequest {
            speaker: Some("ghost".into()),
            ..req("greeting")
        }),
        "dialogue_unknown_voice"
    );
    assert_eq!(code(req("only_guard")), "dialogue_no_variant");
}

#[test]
fn the_sound_follows_the_audio_chain_and_cues_come_inline_or_from_the_sidecar() {
    let lib = library();
    let mut l = loc();
    let r = resolve_line(&lib, Some(&l), &req("greeting")).expect("en");
    assert_eq!(r.resolved_sound, "audio/greeting.ogg");
    l.set_audio_locale(Some("ja")).expect("ja");
    let r = resolve_line(&lib, Some(&l), &req("greeting")).expect("ja");
    assert_eq!(r.resolved_sound, "localized/ja/audio/greeting.ogg");
    assert_eq!(
        r.text, "Ranger greets her friend.",
        "text stays in the text locale"
    );
    let r = resolve_line(&lib, Some(&l), &req("cued")).expect("cued");
    assert_eq!(r.cues.len(), 2);
    assert_eq!(r.cues[1].start_us, 1_500_000);
    let r = resolve_line(&lib, Some(&l), &req("sidecar")).expect("sidecar");
    assert_eq!(r.cues[0].text.as_deref(), Some("side cue"));
    assert_eq!(r.track_priority, Some(2));
}

fn captions(app: &App) -> Vec<CaptionStart> {
    app.world()
        .resource::<Messages<CaptionStart>>()
        .iter_current_update_messages()
        .cloned()
        .collect()
}

fn events(app: &App) -> Vec<DialogueEventKind> {
    app.world()
        .resource::<Messages<DialogueEvent>>()
        .iter_current_update_messages()
        .map(|e| e.kind)
        .collect()
}

#[test]
fn a_line_plays_its_subtitle_and_finishes_uncut() {
    let mut app = app();
    app.update();
    let handle = play_line(app.world_mut(), &req("cued")).expect("plays");
    let caption = captions(&app);
    assert_eq!(caption.len(), 1);
    let texts: Vec<String> = caption[0]
        .pieces
        .iter()
        .map(|p| match &p.text {
            CaptionText::Literal(t) => t.clone(),
            CaptionText::Key { key, .. } => key.clone(),
        })
        .collect();
    // The first cue ends at 1 s and the next starts at 1.5 s: the gap is an empty piece.
    assert_eq!(texts, ["First cue", "", "[laughs]"]);
    assert_eq!(caption[0].pieces[1].offset_us, 1_000_000);
    assert_eq!(caption[0].pieces[2].offset_us, 1_500_000);
    app.update();
    assert_eq!(events(&app), [DialogueEventKind::Started]);
    let end = app.world().resource::<DialoguePlayback>().active[&handle].end_us;
    // Not finished one microsecond early; finished at the planned end.
    app.world_mut().resource_mut::<CaptionClock>().0 = end - 1;
    app.update();
    assert!(events(&app).is_empty());
    app.world_mut().resource_mut::<CaptionClock>().0 = end;
    app.update();
    assert_eq!(events(&app), [DialogueEventKind::Finished]);
    assert!(app.world().resource::<DialoguePlayback>().active.is_empty());
    let stops: Vec<CaptionStop> = app
        .world()
        .resource::<Messages<CaptionStop>>()
        .iter_current_update_messages()
        .copied()
        .collect();
    assert_eq!(stops.len(), 1);
}

/// A voice player a test scripts: every clip has the given length and plays until `stopped`.
#[derive(Clone, Default)]
struct ScriptedVoice {
    duration_us: Option<u64>,
    stopped: Arc<AtomicBool>,
    positions: Arc<Mutex<Vec<Option<Vec3>>>>,
}

impl VoicePlayer for ScriptedVoice {
    fn play(&mut self, _clip: &str, _duration_us: Option<u64>) -> Option<VoiceStart> {
        self.positions.lock().expect("no panic").push(None);
        Some(VoiceStart {
            handle: 1,
            start_us: 0,
            duration_us: self.duration_us,
        })
    }
    fn play_at(&mut self, clip: &str, duration_us: Option<u64>, position: Vec3) -> Option<VoiceStart> {
        let start = self.play(clip, duration_us);
        if let Some(last) = self.positions.lock().expect("no panic").last_mut() {
            *last = Some(position);
        }
        start
    }
    fn playing(&self, _handle: u64) -> bool {
        !self.stopped.load(Ordering::Relaxed)
    }
    fn stop(&mut self, _handle: u64, _fade_ms: u32) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

fn scripted_app(voice: &ScriptedVoice) -> App {
    let mut app = app();
    app.insert_resource(ConversationVoice(Some(Box::new(voice.clone()))));
    app.update();
    app
}

#[test]
fn a_position_plays_the_voice_spatially() {
    let voice = ScriptedVoice::default();
    let mut app = scripted_app(&voice);
    let at = Vec3::new(1.0, 2.0, 3.0);
    let mut spatial = req("greeting");
    spatial.position = Some(at);
    let handle = play_line(app.world_mut(), &spatial).expect("plays");
    play_line(app.world_mut(), &req("greeting")).expect("plays");
    assert_eq!(*voice.positions.lock().expect("no panic"), [Some(at), None]);
    let playback = app.world().resource::<DialoguePlayback>();
    assert_eq!(playback.active[&handle].position, Some(at));
}

#[test]
fn a_voice_cut_before_its_end_interrupts_the_line() {
    let voice = ScriptedVoice {
        duration_us: Some(5_000_000),
        ..ScriptedVoice::default()
    };
    let mut app = scripted_app(&voice);
    play_line(app.world_mut(), &req("greeting")).expect("plays");
    app.update();
    assert_eq!(events(&app), [DialogueEventKind::Started]);
    // Stolen from outside the player, at 2 s of 5 s.
    app.world_mut().resource_mut::<CaptionClock>().0 = 2_000_000;
    voice.stopped.store(true, Ordering::Relaxed);
    app.update();
    assert_eq!(events(&app), [DialogueEventKind::Interrupted]);
    assert!(app.world().resource::<DialoguePlayback>().active.is_empty());
}

#[test]
fn a_voice_ending_a_block_early_still_finishes() {
    let voice = ScriptedVoice {
        duration_us: Some(5_000_000),
        ..ScriptedVoice::default()
    };
    let mut app = scripted_app(&voice);
    let handle = play_line(app.world_mut(), &req("greeting")).expect("plays");
    app.update();
    let end = app.world().resource::<DialoguePlayback>().active[&handle].end_us;
    app.world_mut().resource_mut::<CaptionClock>().0 = end - 10_000;
    voice.stopped.store(true, Ordering::Relaxed);
    app.update();
    assert!(events(&app).is_empty(), "the clock catches up with the mixer");
    app.world_mut().resource_mut::<CaptionClock>().0 = end;
    app.update();
    assert_eq!(events(&app), [DialogueEventKind::Finished]);
}

/// An app with the dialogue player but no caption presenter (no [`CaptionClock`]), stepping
/// [`Time`] by 100 ms a frame, with `voice` as the voice player (`None`: no voice and no audio).
fn app_without_captions(voice: Option<Box<dyn VoicePlayer>>) -> App {
    let mut app = App::new();
    app.add_plugins(DialoguePlugin);
    app.insert_resource(library());
    app.insert_resource(loc());
    app.insert_resource(ConversationVoice(voice));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(100),
    ));
    app.update();
    assert!(!app.world().contains_resource::<CaptionClock>());
    app
}

/// The kinds of every dialogue event over `frames` updates, with the frame each came on.
fn run_events(app: &mut App, frames: u32, mut each: impl FnMut(&mut App)) -> Vec<(u32, DialogueEventKind)> {
    let mut out = Vec::new();
    for frame in 0..frames {
        each(app);
        app.update();
        out.extend(events(app).into_iter().map(|k| (frame, k)));
    }
    out
}

/// Without the caption presenter the line's clock follows [`Time`] (audit framework5 #3): a line
/// with no voice finishes at its planned end instead of staying active forever.
#[test]
fn a_line_without_captions_or_voice_finishes_on_game_time() {
    let mut app = app_without_captions(None);
    let handle = play_line(app.world_mut(), &req("greeting")).expect("plays");
    let (start, end) = {
        let p = &app.world().resource::<DialoguePlayback>().active[&handle];
        (p.start_us, p.end_us)
    };
    assert!(end > start, "a planned length from the text estimate");
    let frames = u32::try_from((end - start) / 100_000 + 20).expect("frames");
    let got = run_events(&mut app, frames, |_| {});
    let kinds: Vec<DialogueEventKind> = got.iter().map(|(_, k)| *k).collect();
    assert_eq!(
        kinds,
        [DialogueEventKind::Started, DialogueEventKind::Finished],
        "{got:?}"
    );
    assert!(app.world().resource::<DialoguePlayback>().active.is_empty());
    let finished_at = got[1].0;
    assert!(
        u64::from(finished_at + 2) * 100_000 >= end - start,
        "not before the planned end: frame {finished_at}, {start}..{end}"
    );
}

/// Without the caption presenter, a voice of known length that plays to its end finishes the
/// line; a frozen clock would have called it cut (audit framework5 #3).
#[test]
fn a_voiced_line_without_captions_finishes_when_its_voice_ends() {
    let voice = ScriptedVoice {
        duration_us: Some(1_000_000),
        ..ScriptedVoice::default()
    };
    let mut app = app_without_captions(Some(Box::new(voice.clone())));
    let handle = play_line(app.world_mut(), &req("greeting")).expect("plays");
    let end = app.world().resource::<DialoguePlayback>().active[&handle].end_us;
    let stopped = voice.stopped.clone();
    let got = run_events(&mut app, 30, |app| {
        // The mixer's clip ends when game time reaches the line's end.
        let now = u64::try_from(app.world().resource::<Time>().sim_elapsed().as_micros()).expect("us");
        if now >= end {
            stopped.store(true, Ordering::Relaxed);
        }
    });
    let kinds: Vec<DialogueEventKind> = got.iter().map(|(_, k)| *k).collect();
    assert_eq!(
        kinds,
        [DialogueEventKind::Started, DialogueEventKind::Finished],
        "{got:?}"
    );
}

#[test]
fn unloading_the_level_stops_every_line() {
    let voice = ScriptedVoice::default();
    let mut app = scripted_app(&voice);
    play_line(app.world_mut(), &req("greeting")).expect("plays");
    app.update();
    app.world_mut().run_schedule(OnLevelUnloading);
    assert!(app.world().resource::<DialoguePlayback>().active.is_empty());
    assert!(voice.stopped.load(Ordering::Relaxed));
    app.update();
    assert_eq!(events(&app), [DialogueEventKind::Interrupted]);
}

#[test]
fn a_sidecar_cue_set_keeps_its_line_breaks() {
    let mut library = library();
    library.subtitles.values_mut().for_each(|t| t.keep_breaks = true);
    let mut app = app();
    app.insert_resource(library);
    app.update();
    play_line(app.world_mut(), &req("sidecar")).expect("plays");
    assert!(captions(&app)[0].keep_breaks);
    play_line(app.world_mut(), &req("cued")).expect("plays");
    assert!(!captions(&app)[1].keep_breaks, "inline cues have no such option");
}

#[test]
fn a_sidecar_cue_set_passes_one_at_a_time_to_the_caption() {
    let mut library = library();
    library
        .subtitles
        .values_mut()
        .for_each(|t| t.one_at_a_time = true);
    let resolved = resolve_line(&library, Some(&loc()), &req("sidecar")).expect("sidecar");
    assert!(resolved.one_at_a_time);
    let mut app = app();
    app.insert_resource(library);
    app.update();
    play_line(app.world_mut(), &req("sidecar")).expect("plays");
    assert!(captions(&app)[0].hint.one_at_a_time);
    play_line(app.world_mut(), &req("cued")).expect("plays");
    assert!(
        !captions(&app)[1].hint.one_at_a_time,
        "inline cues have no such option"
    );
}

/// Like the Android APK mount: `list_dir` gives files only, nothing is a plain host path.
struct FlatMount(BTreeMap<String, Vec<u8>>);

impl regia_platform::VfsMount for FlatMount {
    fn read(&self, relative: &str) -> std::io::Result<Vec<u8>> {
        self.0
            .get(relative)
            .cloned()
            .ok_or_else(|| std::io::ErrorKind::NotFound.into())
    }
    fn open(&self, relative: &str) -> std::io::Result<Box<dyn regia_platform::VfsReader>> {
        Ok(Box::new(std::io::Cursor::new(self.read(relative)?)))
    }
    fn exists(&self, relative: &str) -> bool {
        self.0.contains_key(relative)
    }
    fn list_dir(&self, relative: &str) -> std::io::Result<Vec<regia_platform::VfsDirEntry>> {
        Ok(self
            .0
            .keys()
            .filter_map(|k| k.strip_prefix(relative)?.strip_prefix('/'))
            .filter(|name| !name.contains('/'))
            .map(|name| regia_platform::VfsDirEntry {
                name: name.to_owned(),
                is_dir: false,
            })
            .collect())
    }
    fn describe(&self) -> String {
        "flat test mount".to_owned()
    }
}

#[test]
fn a_package_index_loads_through_a_mount_that_cannot_be_walked() {
    let root = regia_test_utils::unique_temp_dir("dialogue-index");
    std::fs::create_dir_all(root.join("dialogue")).expect("mkdir");
    std::fs::create_dir_all(root.join("localized/fr/audio")).expect("mkdir");
    std::fs::create_dir_all(root.join("audio")).expect("mkdir");
    std::fs::write(root.join("dialogue/intro.dialogue.ron"), DIALOGUE).expect("write");
    std::fs::write(
        root.join("dialogue/ranger.voice.ron"),
        voice("ranger", "Feminine"),
    )
    .expect("write");
    std::fs::write(root.join("audio/greeting.ogg"), b"ogg").expect("write");
    std::fs::write(root.join("audio/music.ogg"), b"ogg").expect("write");
    std::fs::write(root.join("localized/fr/audio/greeting.ogg"), b"ogg").expect("write");
    let library = DialogueLibrary::load_dir(&root);
    let index = library.index(&root);
    assert_eq!(
        index.files,
        ["dialogue/intro.dialogue.ron", "dialogue/ranger.voice.ron"]
    );
    assert_eq!(
        index.sounds,
        ["audio/greeting.ogg", "localized/fr/audio/greeting.ogg"],
        "only what a dialogue names, with its variants"
    );
    let mut files = BTreeMap::new();
    files.insert(
        DIALOGUE_INDEX_PATH.to_owned(),
        ron::to_string(&index).expect("ron").into_bytes(),
    );
    for rel in &index.files {
        files.insert(rel.clone(), std::fs::read(root.join(rel)).expect("read"));
    }
    let _ = std::fs::remove_dir_all(&root);
    let mut vfs = Vfs::new();
    vfs.mount(regia_platform::MountPoint::Assets, FlatMount(files.clone()));
    let loaded = DialogueLibrary::load_index(&vfs).expect("an index");
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    assert_eq!(loaded.dialogues, library.dialogues);
    assert_eq!(loaded.voices, library.voices);
    assert!(loaded.sounds.contains("localized/fr/audio/greeting.ogg"));
    // The Startup system takes that path and needs no host directory.
    let mut app = App::new();
    app.add_plugins(DialoguePlugin);
    app.world_mut().insert_resource(vfs);
    app.update();
    assert_eq!(
        app.world().resource::<DialogueLibrary>().dialogues,
        library.dialogues
    );
    // A file the index lists but the package lost is reported, not fatal.
    files.remove("dialogue/ranger.voice.ron");
    let mut vfs = Vfs::new();
    vfs.mount(regia_platform::MountPoint::Assets, FlatMount(files));
    let loaded = DialogueLibrary::load_index(&vfs).expect("an index");
    assert_eq!(loaded.errors.len(), 1);
    // No index: no library from it.
    assert!(DialogueLibrary::load_index(&Vfs::new()).is_none());
}

#[test]
fn stopping_interrupts_and_a_locale_switch_mid_line_keeps_the_running_text() {
    let mut app = app();
    app.update();
    let first = play_line(app.world_mut(), &req("greeting")).expect("plays");
    let before = captions(&app);
    app.world_mut().resource_mut::<Localization>().set_locale("fr");
    app.update();
    // The running caption keeps its resolved English text; a new line uses French.
    assert!(matches!(&before[0].pieces[0].text, CaptionText::Literal(t) if t == "Hello!"));
    play_line(app.world_mut(), &req("greeting")).expect("again");
    let after = captions(&app);
    assert!(matches!(&after[0].pieces[0].text, CaptionText::Literal(t) if t == "Bonjour !"));
    assert_eq!(stop_line(app.world_mut(), Some(first), 100).expect("stop"), 1);
    assert_eq!(
        stop_line(app.world_mut(), Some(first), 0).map_err(|e| e.code()),
        Err("dialogue_bad_handle")
    );
    app.update();
    let got = events(&app);
    assert!(got.contains(&DialogueEventKind::Interrupted), "{got:?}");
    assert!(!got.contains(&DialogueEventKind::Finished));
    assert_eq!(stop_line(app.world_mut(), None, 0).expect("all"), 1);
}

#[test]
fn the_library_loads_a_directory_and_lists_bad_files() {
    let root = regia_test_utils::unique_temp_dir("dialogue-library");
    std::fs::create_dir_all(root.join("dialogue")).expect("mkdir");
    std::fs::create_dir_all(root.join("localized/fr/audio")).expect("mkdir");
    std::fs::write(root.join("dialogue/ranger_intro.dialogue.ron"), DIALOGUE).expect("write");
    std::fs::write(
        root.join("dialogue/ranger.voice.ron"),
        voice("ranger", "Feminine"),
    )
    .expect("write");
    std::fs::write(
        root.join("dialogue/bad.voice.ron"),
        "(format_version: 1, id: \"Bad\", name_key: \"k\")",
    )
    .expect("write");
    std::fs::write(root.join("localized/fr/audio/greeting.ogg"), b"x").expect("write");
    let lib = DialogueLibrary::load_dir(&root);
    assert!(lib.dialogues.contains_key("ranger_intro"));
    assert!(lib.voices.contains_key("ranger"));
    assert!(lib.sounds.contains("localized/fr/audio/greeting.ogg"));
    assert_eq!(lib.errors.len(), 1, "{:?}", lib.errors);
    assert!(
        lib.errors[0].starts_with("dialogue/bad.voice.ron:"),
        "{:?}",
        lib.errors
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A second dialogue with a name already loaded, or a second voice with a known id, is reported
/// and skipped: the first file (by sorted path) keeps its lines (audit framework5 #13).
#[test]
fn a_second_dialogue_or_voice_with_the_same_id_is_reported_and_the_first_kept() {
    let root = regia_test_utils::unique_temp_dir("dialogue-duplicate-ids");
    for dir in ["a", "b"] {
        std::fs::create_dir_all(root.join(dir)).expect("mkdir");
    }
    std::fs::write(root.join("a/first.dialogue.ron"), DIALOGUE).expect("write");
    std::fs::write(
        root.join("b/second.dialogue.ron"),
        r#"(format_version: 1, name: "ranger_intro", lines: [(id: "other", speaker: "ranger", variants: [(sound: "audio/o.ogg")])])"#,
    )
    .expect("write");
    std::fs::write(root.join("a/ranger.voice.ron"), voice("ranger", "Feminine")).expect("write");
    std::fs::write(root.join("b/ranger2.voice.ron"), voice("ranger", "Masculine")).expect("write");
    let lib = DialogueLibrary::load_dir(&root);
    let first = &lib.dialogues["ranger_intro"];
    assert!(
        first.lines.iter().any(|l| l.id == "greeting"),
        "the first file's lines"
    );
    assert!(first.lines.iter().all(|l| l.id != "other"));
    assert_eq!(
        lib.voices["ranger"].gender,
        regia_asset::dialogue::Gender::Feminine,
        "the first voice file"
    );
    assert_eq!(lib.errors.len(), 2, "{:?}", lib.errors);
    // Sorted paths: `b/ranger2.voice.ron` before `b/second.dialogue.ron`.
    assert!(
        lib.errors[0].starts_with("b/ranger2.voice.ron:"),
        "{:?}",
        lib.errors
    );
    assert!(
        lib.errors[1].starts_with("b/second.dialogue.ron:"),
        "{:?}",
        lib.errors
    );
    let has = |_: &str| true;
    let unknown_length = |_: &str| -> Option<u64> { None };
    let env = DialogueValidateEnv {
        has_source_key: &has,
        audio_chains: &[],
        sound_us: &unknown_length,
    };
    let issues = validate_dialogues(&lib, &env);
    let duplicates: Vec<&str> = issues
        .iter()
        .filter(|i| i.code == "dialogue_duplicate_id")
        .map(|i| i.subject.as_str())
        .collect();
    assert_eq!(
        duplicates,
        ["b/ranger2.voice.ron", "b/second.dialogue.ron"],
        "{issues:?}"
    );
    assert!(
        issues
            .iter()
            .filter(|i| i.code == "dialogue_duplicate_id")
            .all(|i| i.error)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn validators_report_unknown_voices_missing_text_sound_and_late_cues() {
    let mut lib = library();
    lib.errors
        .push("dialogue/x.dialogue.ron:3:1: duplicate line id \"a\"".into());
    let l = loc();
    let has = |k: &str| l.has_in("en", k);
    let chains = vec![
        ("en".to_owned(), vec!["en".to_owned()]),
        ("ja".to_owned(), vec!["ja".to_owned(), "en".to_owned()]),
    ];
    let lengths = |s: &str| (s == "audio/cued.ogg").then_some(1_000_000);
    let env = DialogueValidateEnv {
        has_source_key: &has,
        audio_chains: &chains,
        sound_us: &lengths,
    };
    let issues = validate_dialogues(&lib, &env);
    let count = |code: &str| issues.iter().filter(|i| i.code == code).count();
    assert_eq!(count("dialogue_duplicate_line"), 1);
    assert_eq!(count("dialogue_unknown_voice"), 1, "guard: {issues:?}");
    // greeting_child.ogg and g.ogg have no file in either locale.
    assert_eq!(count("dialogue_missing_sound"), 4, "{issues:?}");
    assert_eq!(
        count("subtitle_past_end"),
        1,
        "the [laughs] cue at 1.5 s of a 1 s sound"
    );
    assert!(
        issues.iter().all(|i| i.code != "dialogue_missing_text"),
        "{issues:?}"
    );
    assert!(
        !issues
            .iter()
            .find(|i| i.code == "subtitle_past_end")
            .expect("late")
            .error
    );
}
