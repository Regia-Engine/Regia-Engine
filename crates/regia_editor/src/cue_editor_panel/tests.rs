// SPDX-License-Identifier: MPL-2.0
//! Cue editor tests: open, edit with undo, byte-identical saves, audition, the agent call and headless drawing.

use serde_json::json;

use super::*;
use crate::data_panels::test_ui::draw_frames;

fn method(name: &str) -> Handler {
    methods()
        .into_iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| h)
        .expect("registered")
}

const REL: &str = "audio/footstep.cue.ron";

fn cue() -> SoundCueDef {
    SoundCueDef {
        version: regia_audio::CUE_VERSION,
        name: "footstep".into(),
        bus: Some("World".into()),
        sounds: vec![
            regia_audio::CueSoundDef {
                sound: "audio/step_a.wav".into(),
                weight: 1.0,
            },
            regia_audio::CueSoundDef {
                sound: "audio/step_b.wav".into(),
                weight: 3.0,
            },
        ],
        mode: CuePlayMode::Shuffle,
        no_repeat_last: 1,
        pitch_min: 0.95,
        pitch_max: 1.05,
        volume_db_min: -2.0,
        volume_db_max: 0.0,
        max_concurrent: Some(4),
        steal: StealPolicy::Quietest,
        cooldown: Duration::from_millis(80),
        volume_db: -3.0,
        filter_cutoff_hz: None,
        filter_offset_octaves: 0.0,
        caption: None,
    }
}

/// A world with a project holding the canonical cue file. Returns the world and the file's text.
fn fixture(text_override: Option<&str>) -> (World, PathBuf, String) {
    let root = regia_test_utils::unique_temp_dir("cue-editor");
    let text = text_override.map_or_else(|| cue().to_ron().expect("serialize"), str::to_owned);
    let file = root.join(REL);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, &text).expect("write");
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject { root: Some(root) });
    world.insert_resource(UndoStack::new());
    (world, file, text)
}

#[test]
fn the_serializer_round_trips_a_canonical_cue_byte_identically() {
    let text = cue().to_ron().expect("serialize");
    let parsed = SoundCueDef::from_ron(&text).expect("parse");
    assert_eq!(parsed, cue());
    assert_eq!(parsed.to_ron().expect("serialize again"), text);
}

#[test]
fn an_unedited_cue_saves_with_the_same_bytes() {
    // A hand-written file the serializer would format differently: saving without an edit must not touch it.
    let hand = "(version: 3, name: \"footstep\", sounds: [(sound: \"audio/step_a.wav\")])";
    let (mut world, file, _) = fixture(Some(hand));
    open_cue(&mut world, REL).expect("open");
    assert_eq!(save_cue(&mut world).expect("save"), 0, "nothing written");
    assert_eq!(std::fs::read_to_string(&file).expect("read"), hand);

    // A canonical file saves identically too, through the real write path after a no-op edit pair.
    let (mut world, file, text) = fixture(None);
    open_cue(&mut world, REL).expect("open");
    edit_cue(&mut world, &CueChange::Weight(0, 2.0)).expect("edit");
    assert!(crate::command::undo_world(&mut world));
    assert!(!world.resource::<CueEditorPanel>().doc().expect("doc").dirty());
    assert_eq!(save_cue(&mut world).expect("save"), 0);
    assert_eq!(std::fs::read(&file).expect("read"), text.as_bytes());
    // Forcing the serializer on the canonical definition reproduces the file.
    let def = world.resource::<CueEditorPanel>().doc().expect("doc").def.clone();
    assert_eq!(def.to_ron().expect("serialize"), text);
}

#[test]
fn edits_are_undoable_save_and_reload() {
    let (mut world, file, text) = fixture(None);
    open_cue(&mut world, REL).expect("open");
    assert!(edit_cue(&mut world, &CueChange::Weight(1, 5.0)).expect("weight"));
    assert!(edit_cue(&mut world, &CueChange::AddSound("audio/step_c.wav".into(), 2.0)).expect("add"));
    assert!(edit_cue(&mut world, &CueChange::Mode(CuePlayMode::Random)).expect("mode"));
    assert!(edit_cue(&mut world, &CueChange::Steal(StealPolicy::None)).expect("steal"));
    assert!(edit_cue(&mut world, &CueChange::Pitch(0.9, 1.1)).expect("pitch"));
    assert!(edit_cue(&mut world, &CueChange::VolumeRange(-4.0, 1.0)).expect("volume"));
    assert!(world.resource::<CueEditorPanel>().doc().expect("doc").dirty());
    let written = save_cue(&mut world).expect("save");
    assert!(written > 0);
    let saved = std::fs::read_to_string(&file).expect("read");
    assert_ne!(saved, text);
    let parsed = SoundCueDef::from_ron(&saved).expect("the written file parses");
    assert_eq!(parsed.sounds.len(), 3);
    assert_eq!(parsed.sounds[1].weight, 5.0);
    assert_eq!(parsed.mode, CuePlayMode::Random);
    assert_eq!(parsed.steal, StealPolicy::None);
    assert_eq!((parsed.pitch_min, parsed.pitch_max), (0.9, 1.1));
    assert_eq!((parsed.volume_db_min, parsed.volume_db_max), (-4.0, 1.0));
    assert_eq!(parsed.version, regia_audio::CUE_VERSION, "the shape is unchanged");
    // Undo all the way back to the original definition.
    while crate::command::undo_world(&mut world) {}
    let doc = world.resource::<CueEditorPanel>().doc().expect("doc").clone();
    assert_eq!(doc.def, cue());
    assert!(doc.dirty(), "the file now holds the edited version");
}

#[test]
fn bad_changes_are_refused_with_codes_and_leave_the_cue_alone() {
    let (mut world, _, _) = fixture(None);
    open_cue(&mut world, REL).expect("open");
    for (change, code) in [
        (CueChange::Pitch(1.2, 0.8), "cue_editor_bad_value"),
        (CueChange::Pitch(f32::NAN, 1.0), "cue_editor_bad_value"),
        (CueChange::Pitch(0.0, 1.0), "cue_editor_bad_value"),
        (CueChange::VolumeRange(2.0, -2.0), "cue_editor_bad_value"),
        (CueChange::Weight(0, -1.0), "cue_editor_bad_value"),
        (CueChange::Weight(9, 1.0), "cue_editor_bad_index"),
        (CueChange::RemoveSound(9), "cue_editor_bad_index"),
        (CueChange::AddSound(String::new(), 1.0), "cue_editor_bad_value"),
    ] {
        let e = edit_cue(&mut world, &change).expect_err("refused");
        assert_eq!(e.code, code, "{change:?}");
    }
    assert!(!world.resource::<CueEditorPanel>().doc().expect("doc").dirty());
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
    let e = open_cue(&mut world, "../escape.cue.ron").expect_err("escape");
    assert_eq!(e.code, "cue_editor_bad_path");
    let e = open_cue(&mut world, "audio/missing.cue.ron").expect_err("missing");
    assert_eq!(e.code, "cue_editor_io_failed");
}

#[test]
fn audition_picks_by_weight_and_walks_sequential_cues() {
    let def = cue();
    let mut hits = [0usize; 2];
    for n in 0..2000 {
        hits[pick_variation(&def, n).expect("pick")] += 1;
    }
    let share = hits[1] as f32 / 2000.0;
    assert!(
        (share - 0.75).abs() < 0.05,
        "weight 3 of 4 picks ~75 %, got {share}"
    );
    assert_eq!(pick_variation(&def, 7), pick_variation(&def, 7), "deterministic");
    let mut seq = cue();
    seq.mode = CuePlayMode::Sequential;
    let order: Vec<usize> = (0..4).map(|n| pick_variation(&seq, n).expect("pick")).collect();
    assert_eq!(order, vec![0, 1, 0, 1]);
    let mut empty = cue();
    empty.sounds.iter_mut().for_each(|s| s.weight = 0.0);
    assert_eq!(pick_variation(&empty, 0), None);

    let (mut world, _, _) = fixture(None);
    open_cue(&mut world, REL).expect("open");
    let a = audition(&mut world, false).expect("audition");
    assert!((0.95..=1.05).contains(&a.pitch));
    assert!(
        (-5.0..=-3.0).contains(&a.volume_db),
        "base -3 plus a draw in -2..0: {}",
        a.volume_db
    );
    assert!(!a.played);
}

#[test]
fn the_agent_call_edits_saves_and_auditions() {
    let (mut world, file, _) = fixture(None);
    let got = method("editor.cue_edit")(&mut world, &json!({ "path": REL })).expect("open");
    assert_eq!(got["open"], json!(true));
    assert_eq!(got["sounds"].as_array().expect("sounds").len(), 2);
    assert_eq!(got["dirty"], json!(false));
    let set = cue_edit(
        &mut world,
        &json!({ "op": "set", "mode": "sequential", "pitch_min": 0.8, "max_concurrent": null }),
    )
    .expect("set");
    assert_eq!(set["mode"], json!("sequential"));
    assert_eq!(set["max_concurrent"], json!(null));
    let pitch = set["pitch"].as_array().expect("pitch");
    assert!((pitch[0].as_f64().expect("min") - 0.8).abs() < 1e-6);
    assert!((pitch[1].as_f64().expect("max") - 1.05).abs() < 1e-6);
    method("editor.cue_edit")(
        &mut world,
        &json!({ "op": "add_sound", "sound": "audio/step_c.wav", "weight": 2 }),
    )
    .expect("add");
    method("editor.cue_edit")(
        &mut world,
        &json!({ "op": "set_weight", "index": 0, "weight": 4 }),
    )
    .expect("weight");
    let aud = method("editor.cue_edit")(&mut world, &json!({ "op": "audition" })).expect("audition");
    assert!(aud["audition"]["sound"].is_string());
    let saved = method("editor.cue_edit")(&mut world, &json!({ "op": "save" })).expect("save");
    assert!(saved["written"].as_u64().expect("written") > 0);
    assert_eq!(saved["dirty"], json!(false));
    let parsed = SoundCueDef::from_ron(&std::fs::read_to_string(&file).expect("read")).expect("parse");
    assert_eq!(parsed.sounds.len(), 3);
    assert_eq!(parsed.sounds[0].weight, 4.0);
    // A failing set changes nothing (all or nothing).
    let depth = world.resource::<UndoStack>().undo_depth();
    let e = method("editor.cue_edit")(
        &mut world,
        &json!({ "op": "set", "mode": "shuffle", "pitch_min": -1 }),
    )
    .expect_err("bad");
    assert!(format!("{e:?}").contains("cue_editor_bad_value"), "{e:?}");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth);
    assert!(method("editor.cue_edit")(&mut world, &json!({ "op": "dance" })).is_err());
    let mut fresh = World::new();
    let e = method("editor.cue_edit")(&mut fresh, &json!({})).expect_err("no cue");
    assert!(format!("{e:?}").contains("cue_editor_no_cue"), "{e:?}");
}

#[test]
fn caption_and_filter_set_save_reload_round_trip() {
    let (mut world, file, text) = fixture(None);
    let get = method("editor.cue_edit");
    get(&mut world, &json!({ "path": REL })).expect("open");
    let set = get(
        &mut world,
        &json!({
            "op": "set",
            "caption": { "key": "sfx-footstep", "kind": "speech", "priority": 3, "planned_ms": 1200 },
            "filter_cutoff_hz": 8000.0,
            "filter_offset_octaves": -1.0,
        }),
    )
    .expect("set");
    assert_eq!(set["caption"]["key"], json!("sfx-footstep"));
    assert_eq!(set["caption"]["kind"], json!("speech"));
    assert_eq!(set["caption"]["priority"], json!(3));
    assert_eq!(set["caption"]["planned_ms"], json!(1200));
    assert_eq!(set["filter_cutoff_hz"], json!(8000.0));
    assert_eq!(set["filter_offset_octaves"], json!(-1.0));
    // Save, reload: the caption and filter survive the round trip.
    get(&mut world, &json!({ "op": "save" })).expect("save");
    let on_disk = SoundCueDef::from_ron(&std::fs::read_to_string(&file).expect("read")).expect("parse");
    let expected_caption = regia_audio::CueCaptionDef {
        key: "sfx-footstep".into(),
        kind: regia_audio::CueCaptionKind::Speech,
        priority: 3,
        planned_ms: Some(1200),
    };
    assert_eq!(on_disk.caption, Some(expected_caption.clone()));
    assert_eq!(on_disk.filter_cutoff_hz, Some(8000.0));
    assert_eq!(on_disk.filter_offset_octaves, -1.0);
    let reopened = get(&mut world, &json!({ "path": REL })).expect("reopen");
    assert_eq!(reopened["caption"]["key"], json!("sfx-footstep"));
    assert_eq!(reopened["filter_cutoff_hz"], json!(8000.0));
    // Undo (two steps: the caption edit and the filter edit) restores the original bytes.
    assert!(crate::command::undo_world(&mut world));
    assert!(crate::command::undo_world(&mut world));
    let doc = world.resource::<CueEditorPanel>().doc().expect("doc").clone();
    assert_eq!(doc.def, cue());
    // As in `edits_are_undoable_save_and_reload`: after the save the file holds the
    // edited version, so the undone document differs from it until it is saved again.
    assert!(doc.dirty(), "the file now holds the edited version");
    assert_eq!(doc.def.to_ron().expect("serialize"), text);
    // Saving the undone document writes the original bytes back.
    get(&mut world, &json!({ "op": "save" })).expect("save the undone cue");
    assert_eq!(std::fs::read_to_string(&file).expect("read"), text);
    assert!(!world.resource::<CueEditorPanel>().doc().expect("doc").dirty());
    // A caption with an empty key and a non-positive cutoff are refused and change nothing.
    for params in [
        json!({ "op": "set", "caption": { "key": "  " } }),
        json!({ "op": "set", "filter_cutoff_hz": -5.0 }),
    ] {
        let e = get(&mut world, &params).expect_err("refused");
        assert!(
            format!("{e:?}").contains("cue_editor_bad_value"),
            "{params}: {e:?}"
        );
        assert!(!world.resource::<CueEditorPanel>().doc().expect("doc").dirty());
    }
    // A non-finite number cannot cross JSON; it is refused at the change level, like Pitch.
    let e = edit_cue(
        &mut world,
        &CueChange::Filter {
            cutoff_hz: Some(f32::NAN),
            offset_octaves: 0.0,
        },
    )
    .expect_err("non-finite");
    assert_eq!(e.code, "cue_editor_bad_value");
    // `caption: null` clears it; `filter_cutoff_hz: null` removes the filter.
    let set = get(
        &mut world,
        &json!({ "op": "set", "caption": { "key": "sfx-step" }, "filter_cutoff_hz": 4000.0 }),
    )
    .expect("set");
    assert_eq!(set["caption"]["key"], json!("sfx-step"));
    let set = get(
        &mut world,
        &json!({ "op": "set", "caption": null, "filter_cutoff_hz": null }),
    )
    .expect("clear");
    assert_eq!(set["caption"], json!(null));
    assert_eq!(set["filter_cutoff_hz"], json!(null));
    assert_eq!(set["filter_offset_octaves"], json!(0.0));
    let e = get(
        &mut world,
        &json!({ "op": "set", "caption": { "key": "k", "kind": "nope" } }),
    )
    .expect_err("kind");
    assert!(format!("{e:?}").contains("kind"), "{e:?}");
}

#[test]
fn the_panel_draws_headless_with_and_without_a_cue() {
    let (mut world, _, _) = fixture(None);
    let empty = draw_frames(&mut world, 2, cue_editor_panel);
    assert!(!empty.is_empty());
    open_cue(&mut world, REL).expect("open");
    let shapes = draw_frames(&mut world, 3, cue_editor_panel);
    assert!(!shapes.is_empty());
    // Drawing made no edit.
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
}

/// w10-editor-agent-smalls (2): the audition reads every format `regia_audio` decodes (the Ogg
/// Vorbis, FLAC and MP3 fixtures here), not only WAV, and refuses a path that leaves the project or a
/// file that is no sound.
#[test]
fn the_audition_decodes_every_sound_format() {
    let dir = regia_test_utils::unique_temp_dir("cue_editor_decode");
    std::fs::create_dir_all(dir.join("audio")).expect("dirs");
    let fixtures: [(&str, &[u8]); 3] = [
        (
            "audio/t.ogg",
            include_bytes!("../../../regia_audio/tests/fixtures/tone_1k.ogg"),
        ),
        (
            "audio/t.flac",
            include_bytes!("../../../regia_audio/tests/fixtures/tone_1k.flac"),
        ),
        (
            "audio/t.mp3",
            include_bytes!("../../../regia_audio/tests/fixtures/tone_1k.mp3"),
        ),
    ];
    for (rel, bytes) in fixtures {
        std::fs::write(dir.join(rel), bytes).expect("write");
        let sound = decode_file(&dir, rel).unwrap_or_else(|| panic!("{rel} decodes"));
        assert!(sound.frames() > 0, "{rel}");
    }
    std::fs::write(dir.join("audio/junk.ogg"), b"not music").expect("write");
    assert!(decode_file(&dir, "audio/junk.ogg").is_none());
    assert!(decode_file(&dir, "../audio/t.ogg").is_none());
    assert!(decode_file(&dir, "audio/missing.ogg").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
