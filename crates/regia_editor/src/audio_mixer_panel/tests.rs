// SPDX-License-Identifier: MPL-2.0
//! Audio mixer dock tests: the model, solo logic, fader undo, the pump into a real `Audio`, the meters from a
//! played tone, the agent calls and headless drawing.

use regia_audio::{Audio, AudioSettings, SoundClip};
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

fn world_with_audio() -> World {
    let mut world = World::new();
    world.insert_resource(Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    }));
    world.insert_resource(UndoStack::new());
    world
}

fn volume_of(world: &World, bus: &str) -> f32 {
    world
        .resource::<AudioMixerPanel>()
        .bus(bus)
        .expect("bus")
        .volume_db
}

#[test]
fn the_model_is_the_default_bus_tree() {
    let mut world = World::new();
    let panel = take_panel(&mut world);
    let names: Vec<&str> = panel.buses().iter().map(|b| b.name.as_str()).collect();
    for want in ["Master", "Music", "Sfx", "World", "Ui", "Voice", "Ambience"] {
        assert!(names.contains(&want), "missing {want}: {names:?}");
    }
    assert_eq!(panel.bus("Master").expect("master").parent, None);
    assert_eq!(panel.bus("World").expect("world").parent.as_deref(), Some("Sfx"));
}

#[test]
fn fader_set_then_undo_restores_the_volume() {
    let mut world = world_with_audio();
    let before = {
        let panel = take_panel(&mut world);
        let v = panel.bus("Music").expect("music").volume_db;
        world.insert_resource(panel);
        v
    };
    let changed = edit_bus(
        &mut world,
        "Music",
        EditRequest {
            volume_db: Some(-17.5),
            ..EditRequest::default()
        },
    )
    .expect("edit");
    assert!(changed);
    assert_eq!(volume_of(&world, "Music"), -17.5);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    assert!(crate::command::undo_world(&mut world));
    assert_eq!(volume_of(&world, "Music"), before);
    assert!(crate::command::redo_world(&mut world));
    assert_eq!(volume_of(&world, "Music"), -17.5);
}

#[test]
fn a_scrub_is_one_undo_step_and_an_unchanged_edit_adds_none() {
    let mut world = world_with_audio();
    for db in [-3.0, -4.0, -5.0] {
        edit_bus(
            &mut world,
            "Sfx",
            EditRequest {
                volume_db: Some(db),
                ..EditRequest::default()
            },
        )
        .expect("edit");
    }
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        1,
        "volume moves of one bus merge"
    );
    let again = edit_bus(
        &mut world,
        "Sfx",
        EditRequest {
            volume_db: Some(-5.0),
            ..EditRequest::default()
        },
    )
    .expect("edit");
    assert!(!again);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
}

#[test]
fn bad_edits_are_refused_with_codes() {
    let mut world = world_with_audio();
    let unknown = edit_bus(&mut world, "Nope", EditRequest::default()).expect_err("unknown");
    assert_eq!(unknown.code, "audio_mixer_unknown_bus");
    for bad in [f32::NAN, f32::INFINITY, -200.0, 99.0] {
        let e = edit_bus(
            &mut world,
            "Music",
            EditRequest {
                volume_db: Some(bad),
                ..EditRequest::default()
            },
        )
        .expect_err("bad value");
        assert_eq!(e.code, "audio_mixer_bad_value");
    }
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
}

#[test]
fn solo_keeps_the_soloed_bus_its_ancestors_and_descendants() {
    let mut world = world_with_audio();
    edit_bus(
        &mut world,
        "Sfx",
        EditRequest {
            solo: Some(true),
            ..EditRequest::default()
        },
    )
    .expect("solo");
    let panel = world.resource::<AudioMixerPanel>();
    assert!(!panel.effective_muted("Sfx"));
    assert!(!panel.effective_muted("Master"), "the ancestor stays audible");
    assert!(!panel.effective_muted("World"), "a descendant stays audible");
    assert!(panel.effective_muted("Music"));
    assert!(panel.effective_muted("Voice"));
    // A user mute beats the solo.
    edit_bus(
        &mut world,
        "World",
        EditRequest {
            mute: Some(true),
            ..EditRequest::default()
        },
    )
    .expect("mute");
    assert!(world.resource::<AudioMixerPanel>().effective_muted("World"));
    // Undo twice: no solo, nothing muted.
    assert!(crate::command::undo_world(&mut world));
    assert!(crate::command::undo_world(&mut world));
    let panel = world.resource::<AudioMixerPanel>();
    assert!(panel.buses().iter().all(|b| !panel.effective_muted(&b.name)));
}

#[test]
fn the_pump_sends_faders_mutes_and_solos_to_the_mixer() {
    let mut world = world_with_audio();
    edit_bus(
        &mut world,
        "Music",
        EditRequest {
            volume_db: Some(-12.0),
            ..EditRequest::default()
        },
    )
    .expect("edit");
    edit_bus(
        &mut world,
        "Voice",
        EditRequest {
            solo: Some(true),
            ..EditRequest::default()
        },
    )
    .expect("solo");
    pump(&mut world, None);
    let audio = world.resource::<Audio>();
    assert_eq!(audio.bus_volume("Music").expect("music"), -12.0);
    assert_eq!(
        audio.bus_muted("Music"),
        Some(true),
        "soloing Voice silences Music"
    );
    assert_eq!(audio.bus_muted("Voice"), Some(false));
    assert_eq!(world.resource::<AudioMixerPanel>().source, Source::Edit);
}

#[test]
fn live_meters_follow_a_played_tone_through_the_pump() {
    let mut world = world_with_audio();
    let clip = SoundClip::new(
        (0..48_000)
            .map(|i| ((i as f32 / 48_000.0) * 1_000.0 * std::f32::consts::TAU).sin() * 0.5)
            .collect::<Vec<_>>(),
        48_000,
    )
    .expect("tone");
    {
        let mut audio = world.resource_mut::<Audio>();
        audio.play(&clip).expect("enqueue");
        audio.drain();
        audio.render_offline(9_600).expect("render");
    }
    // The first pump creates the panel from the model; the second reads the meters it saw.
    world.insert_resource(AudioMixerPanel::default());
    pump(&mut world, None);
    let panel = world.resource::<AudioMixerPanel>();
    let master = panel.meter("Master");
    assert!(
        master.peak_db > -12.0 && master.peak_db < 0.5,
        "Master peak {}",
        master.peak_db
    );
    assert_eq!(
        panel.meter("Music").peak_db,
        METER_FLOOR_DB,
        "silence reads the floor"
    );
}

#[test]
fn agent_get_and_set_round_trip_with_undo() {
    let mut world = world_with_audio();
    let got = method("editor.audio_mixer_get")(&mut world, &json!({})).expect("get");
    assert_eq!(got["floor_db"], json!(METER_FLOOR_DB));
    let music = got["buses"]
        .as_array()
        .expect("buses")
        .iter()
        .find(|b| b["name"] == "Music")
        .expect("Music")
        .clone();
    let was = music["volume_db"].as_f64().expect("db");
    let set = method("editor.audio_mixer_set")(
        &mut world,
        &json!({ "bus": "Music", "volume_db": -9.0, "mute": true }),
    )
    .expect("set");
    assert_eq!(set["changed"], json!(true));
    assert_eq!(set["undo_depth"], json!(1));
    assert!(crate::command::undo_world(&mut world));
    let after = method("editor.audio_mixer_get")(&mut world, &json!({})).expect("get");
    let music = after["buses"]
        .as_array()
        .expect("buses")
        .iter()
        .find(|b| b["name"] == "Music")
        .expect("Music")
        .clone();
    assert_eq!(music["volume_db"].as_f64(), Some(was));
    assert_eq!(music["muted"], json!(false));
    let err = method("editor.audio_mixer_set")(&mut world, &json!({ "bus": "Nope" })).expect_err("unknown");
    assert!(format!("{err:?}").contains("audio_mixer_unknown_bus"), "{err:?}");
    let err =
        method("editor.audio_mixer_set")(&mut world, &json!({ "volume_db": 1 })).expect_err("missing bus");
    assert!(format!("{err:?}").contains("bus"));
}

#[test]
fn the_dock_draws_headless() {
    let mut world = world_with_audio();
    let shapes = draw_frames(&mut world, 3, audio_mixer_panel);
    assert!(!shapes.is_empty(), "the dock draws the bus tree");
    // Drawing created the model.
    assert!(world.resource::<AudioMixerPanel>().bus("Master").is_some());
}

/// The dock's program-loudness row (ADR-7227): the pump turns the output's meter on, the reading
/// after a played tone matches the offline loudness function on the rendered output, the agent get
/// reports it, and Reset starts a new measurement.
#[test]
fn the_program_meter_reads_the_output_and_resets() {
    let mut world = world_with_audio();
    world.insert_resource(AudioMixerPanel::default());
    pump(&mut world, None);
    assert!(
        world.resource::<Audio>().program_loudness().enabled,
        "the pump turns it on"
    );
    let clip = SoundClip::new(
        (0..48_000)
            .map(|i| ((i as f32 / 48_000.0) * 1_000.0 * std::f32::consts::TAU).sin() * 0.1)
            .collect::<Vec<_>>(),
        48_000,
    )
    .expect("tone");
    let out = {
        let mut audio = world.resource_mut::<Audio>();
        audio.play(&clip).expect("enqueue");
        audio.drain();
        audio.render_offline(48_000).expect("render")
    };
    pump(&mut world, None);
    let program = world.resource::<AudioMixerPanel>().program_loudness();
    let offline = regia_audio::measure_loudness(&out, 2, 48_000).expect("valid");
    assert!((program.seconds - 1.0).abs() < 1e-6, "{program:?}");
    assert!(
        (program.integrated_lufs - offline.integrated_lufs).abs() < 0.05,
        "dock {program:?} vs offline {offline:?}"
    );

    let got = method("editor.audio_mixer_get")(&mut world, &json!({})).expect("get");
    assert_eq!(got["program"]["enabled"], true, "{got}");
    assert!(got["program"]["integrated_lufs"].as_f64().is_some(), "{got}");

    world.resource_mut::<AudioMixerPanel>().request_program_reset();
    pump(&mut world, None);
    let after = world.resource::<AudioMixerPanel>().program_loudness();
    assert_eq!(after.seconds, 0.0, "{after:?}");
    assert_eq!(after.integrated_lufs, f32::NEG_INFINITY, "{after:?}");
}

// ------------------------------------------------------------------ chains, sends, snapshots (P-227)

fn tone_clip(amp: f32, secs: f32) -> SoundClip {
    SoundClip::new(
        (0..(48_000.0 * secs) as usize)
            .map(|i| ((i as f32 / 48_000.0) * 1_000.0 * std::f32::consts::TAU).sin() * amp)
            .collect::<Vec<_>>(),
        48_000,
    )
    .expect("tone")
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

/// Ticks the pumped mixer one 1/60 s step and renders that block.
fn step(world: &mut World) -> Vec<f32> {
    let mut audio = world.resource_mut::<Audio>();
    audio.tick(1.0 / 60.0);
    audio.render_offline(800).expect("render")
}

/// The brief's direct Master mute (P-228 had only a mute through solo): the dock's M on Master
/// reaches the mixer through the pump and the whole output is silent; unmuting brings it back.
#[test]
fn master_mute_silences_output() {
    let mut world = world_with_audio();
    world.insert_resource(AudioMixerPanel::default());
    pump(&mut world, None);
    {
        let mut audio = world.resource_mut::<Audio>();
        audio.play(&tone_clip(0.5, 2.0)).expect("enqueue");
        audio.drain();
    }
    let open: Vec<f32> = (0..6).flat_map(|_| step(&mut world)).collect();
    assert!(peak(&open) > 0.1, "the tone plays: {}", peak(&open));
    edit_bus(
        &mut world,
        "Master",
        EditRequest {
            mute: Some(true),
            ..EditRequest::default()
        },
    )
    .expect("mute");
    pump(&mut world, None);
    assert_eq!(world.resource::<Audio>().bus_muted("Master"), Some(true));
    // The mute fades in over about two short blocks; from the fourth block on the output is silence.
    let _fade: Vec<f32> = (0..3).flat_map(|_| step(&mut world)).collect();
    let muted: Vec<f32> = (0..6).flat_map(|_| step(&mut world)).collect();
    assert_eq!(peak(&muted), 0.0, "a muted Master silences every sample");
    pump(&mut world, None);
    assert_eq!(
        world.resource::<AudioMixerPanel>().meter("Master").peak_db,
        METER_FLOOR_DB
    );
    assert!(
        crate::command::undo_world(&mut world),
        "the mute is one undo step"
    );
    pump(&mut world, None);
    let back: Vec<f32> = (0..6).flat_map(|_| step(&mut world)).collect();
    assert!(peak(&back[1_600..]) > 0.1, "unmuted again: {}", peak(&back));
}

#[test]
fn chain_edits_are_undo_steps_and_reach_the_mixer() {
    let mut world = world_with_audio();
    let add = |kind: &str| ChainOp::Add {
        kind: kind.into(),
        index: None,
    };
    assert!(edit_chain(&mut world, "Music", &add("low_pass")).expect("add"));
    assert!(edit_chain(&mut world, "Music", &add("eq")).expect("add"));
    assert!(edit_chain(&mut world, "Music", &ChainOp::Move { from: 1, to: 0 }).expect("move"));
    assert!(
        edit_chain(
            &mut world,
            "Music",
            &ChainOp::Bypass {
                index: 1,
                bypass: true
            }
        )
        .expect("bypass")
    );
    let kinds = |world: &World| -> Vec<(String, bool)> {
        world
            .resource::<AudioMixerPanel>()
            .bus("Music")
            .expect("music")
            .effects
            .iter()
            .map(|e| (e.kind().to_owned(), e.is_bypassed()))
            .collect()
    };
    assert_eq!(
        kinds(&world),
        vec![("eq".to_owned(), false), ("low_pass".to_owned(), true)]
    );
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 4);
    pump(&mut world, None);
    let live = world.resource::<Audio>().bus_layout().clone();
    let music = live.buses.iter().find(|b| b.name == "Music").expect("music");
    assert_eq!(music.effects.len(), 2);
    assert!(music.effects[1].is_bypassed(), "the mixer runs the dock's chain");
    // Undo the bypass and the reorder: the mixer follows at the next pump.
    assert!(crate::command::undo_world(&mut world));
    assert!(crate::command::undo_world(&mut world));
    assert_eq!(
        kinds(&world),
        vec![("low_pass".to_owned(), false), ("eq".to_owned(), false)]
    );
    pump(&mut world, None);
    let live = world.resource::<Audio>().bus_layout().clone();
    let music = live.buses.iter().find(|b| b.name == "Music").expect("music");
    assert_eq!(music.effects[0].kind(), "low_pass");
    // Refusals change nothing.
    let depth = world.resource::<UndoStack>().undo_depth();
    let e = edit_chain(&mut world, "Music", &add("flanger")).expect_err("kind");
    assert_eq!(e.code, "audio_mixer_unknown_effect");
    let e = edit_chain(&mut world, "Music", &ChainOp::Remove { index: 9 }).expect_err("index");
    assert_eq!(e.code, "audio_mixer_bad_index");
    let e = edit_chain(&mut world, "Nope", &add("eq")).expect_err("bus");
    assert_eq!(e.code, "audio_mixer_unknown_bus");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth);
}

#[test]
fn a_dock_send_routes_to_the_reverb_bus_and_a_level_change_is_live() {
    let mut world = world_with_audio();
    assert!(edit_send(&mut world, "Music", "Reverb", Some(-6.0)).expect("add the route"));
    pump(&mut world, None);
    let sends = |world: &World| -> Vec<(String, f32)> {
        world
            .resource::<Audio>()
            .bus_status()
            .into_iter()
            .find(|b| b.name == "Music")
            .expect("music")
            .sends
    };
    assert_eq!(sends(&world), vec![("Reverb".to_owned(), -6.0)]);
    assert!(edit_send(&mut world, "Music", "Reverb", Some(0.0)).expect("raise"));
    pump(&mut world, None);
    assert_eq!(sends(&world), vec![("Reverb".to_owned(), 0.0)]);
    assert!(edit_send(&mut world, "Music", "Reverb", None).expect("remove"));
    pump(&mut world, None);
    assert!(sends(&world).is_empty());
    let e = edit_send(&mut world, "Music", "Nope", Some(0.0)).expect_err("send");
    assert_eq!(e.code, "audio_mixer_unknown_send");
    let e = edit_send(&mut world, "Music", "Reverb", Some(-90.0)).expect_err("range");
    assert_eq!(e.code, "audio_mixer_bad_value");
}

#[test]
fn a_send_scrub_is_one_undo_step_but_adding_the_route_is_its_own() {
    let mut world = world_with_audio();
    let depth = |world: &World| world.resource::<UndoStack>().undo_depth();
    assert!(edit_send(&mut world, "Music", "Reverb", Some(-30.0)).expect("add the route"));
    let after_add = depth(&world);
    for db in [-20.0, -10.0, -3.0] {
        assert!(edit_send(&mut world, "Music", "Reverb", Some(db)).expect("scrub"));
    }
    assert_eq!(depth(&world), after_add + 1, "the three level moves are one step");
    assert!(crate::command::undo_world(&mut world));
    let level = |world: &World| {
        world
            .resource::<AudioMixerPanel>()
            .buses
            .iter()
            .find(|b| b.name == "Music")
            .expect("music")
            .sends
            .clone()
    };
    assert_eq!(level(&world).len(), 1, "undoing the scrub keeps the route");
    assert_eq!(
        level(&world)[0].volume_db,
        -30.0,
        "and restores the level it was added at"
    );
    assert!(crate::command::undo_world(&mut world));
    assert!(level(&world).is_empty(), "the second undo removes the route");
}

#[test]
fn the_dock_applies_a_snapshot_and_its_faders_blend() {
    let mut world = world_with_audio();
    edit_bus(
        &mut world,
        "Music",
        EditRequest {
            volume_db: Some(-24.0),
            ..EditRequest::default()
        },
    )
    .expect("fader");
    assert!(
        edit_snapshot(
            &mut world,
            &SnapshotOp::Save {
                name: "Quiet".into(),
                blend_secs: Some(0.5)
            }
        )
        .expect("save")
    );
    // Back near the layout level; the snapshot keeps -24 dB.
    edit_bus(
        &mut world,
        "Music",
        EditRequest {
            volume_db: Some(-3.0),
            ..EditRequest::default()
        },
    )
    .expect("fader");
    pump(&mut world, None);
    for _ in 0..3 {
        step(&mut world);
    }
    pump(&mut world, None);
    assert_eq!(world.resource::<AudioMixerPanel>().live_db("Music"), Some(-3.0));
    edit_snapshot(&mut world, &SnapshotOp::Apply { name: "Quiet".into() }).expect("apply");
    pump(&mut world, None);
    assert_eq!(
        world.resource::<AudioMixerPanel>().active_snapshot(),
        Some("Quiet")
    );
    let mut path = Vec::new();
    for _ in 0..32 {
        step(&mut world);
        pump(&mut world, None);
        path.push(
            world
                .resource::<AudioMixerPanel>()
                .live_db("Music")
                .expect("music"),
        );
    }
    // 21 dB over 0.5 s at 60 ticks a second.
    assert!(path.windows(2).take(29).all(|w| w[1] < w[0]), "{path:?}");
    assert!((path[14] - -13.5).abs() < 0.05, "halfway: {}", path[14]);
    assert!((path[31] - -24.0).abs() < 1e-3, "arrived: {}", path[31]);
    edit_snapshot(&mut world, &SnapshotOp::Release).expect("release");
    pump(&mut world, None);
    assert_eq!(world.resource::<AudioMixerPanel>().active_snapshot(), None);
    let e = edit_snapshot(&mut world, &SnapshotOp::Release).expect_err("nothing applied");
    assert_eq!(e.code, "audio_mixer_no_snapshot");
    let e = edit_snapshot(&mut world, &SnapshotOp::Apply { name: "Nope".into() }).expect_err("unknown");
    assert_eq!(e.code, "audio_mixer_unknown_snapshot");
    let e = edit_snapshot(
        &mut world,
        &SnapshotOp::Save {
            name: " ".into(),
            blend_secs: None,
        },
    )
    .expect_err("name");
    assert_eq!(e.code, "audio_mixer_bad_name");
}

#[test]
fn save_writes_a_v1_layout_the_runtime_and_the_dock_reload() {
    let root = regia_test_utils::unique_temp_dir("audio-mixer-save");
    let mut world = world_with_audio();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    edit_chain(
        &mut world,
        "Sfx",
        &ChainOp::Add {
            kind: "compressor".into(),
            index: None,
        },
    )
    .expect("add");
    edit_chain(
        &mut world,
        "Sfx",
        &ChainOp::Bypass {
            index: 0,
            bypass: true,
        },
    )
    .expect("bypass");
    edit_snapshot(
        &mut world,
        &SnapshotOp::Save {
            name: "Calm".into(),
            blend_secs: Some(1.0),
        },
    )
    .expect("save");
    assert!(world.resource::<AudioMixerPanel>().unsaved());
    let path = save_layout(&mut world).expect("write");
    assert!(!world.resource::<AudioMixerPanel>().unsaved());
    let text = std::fs::read_to_string(&path).expect("read");
    let layout = BusLayout::parse(&text).expect("the runtime loader reads it");
    assert_eq!(layout.version, regia_audio::AUDIO_BUSES_FORMAT_VERSION);
    let sfx = layout.buses.iter().find(|b| b.name == "Sfx").expect("sfx");
    assert!(sfx.effects[0].is_bypassed() && sfx.effects[0].kind() == "compressor");
    assert_eq!(layout.snapshot("Calm").map(|s| s.blend_secs), Some(1.0));
    // A fresh dock on the same project loads the saved chain and snapshot.
    let mut fresh = World::new();
    fresh.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    let panel = take_panel(&mut fresh);
    assert_eq!(panel.bus("Sfx").expect("sfx").effects, sfx.effects);
    assert_eq!(panel.snapshots().len(), 1);
    let _ = std::fs::remove_dir_all(&root);
    let e = save_layout(&mut World::new()).expect_err("no project");
    assert_eq!(e.code, "audio_mixer_no_project");
}

#[test]
fn agent_chain_send_snapshot_round_trip() {
    let mut world = world_with_audio();
    let got = method("editor.audio_mixer_chain")(
        &mut world,
        &json!({ "bus": "World", "op": "add", "kind": "delay", "index": 0 }),
    )
    .expect("add");
    assert_eq!(got["changed"], json!(true));
    let world_bus = got["buses"]
        .as_array()
        .expect("buses")
        .iter()
        .find(|b| b["name"] == "World")
        .expect("World")
        .clone();
    assert_eq!(world_bus["effects"][0]["kind"], "delay");
    assert_eq!(world_bus["effects"][0]["bypassed"], false);
    assert!(
        world_bus["effects"][0]["params"]["delay_secs"].is_number(),
        "{world_bus}"
    );
    let got = method("editor.audio_mixer_chain")(
        &mut world,
        &json!({ "bus": "World", "op": "bypass", "index": 0 }),
    )
    .expect("bypass");
    let effects = &got["buses"].as_array().expect("buses")[3]["effects"];
    assert_eq!(effects[0]["bypassed"], true, "{effects}");
    let got = method("editor.audio_mixer_send")(
        &mut world,
        &json!({ "bus": "World", "send": "Reverb", "volume_db": -12 }),
    )
    .expect("send");
    assert_eq!(got["send_buses"][0]["name"], "Reverb");
    let got = method("editor.audio_mixer_snapshot")(
        &mut world,
        &json!({ "op": "save", "name": "Hall", "blend_secs": 0.25 }),
    )
    .expect("save");
    assert_eq!(got["snapshots"][0]["name"], "Hall");
    assert_eq!(got["snapshots"][0]["blend_secs"], 0.25);
    let got = method("editor.audio_mixer_snapshot")(&mut world, &json!({ "op": "apply", "name": "Hall" }))
        .expect("apply");
    assert_eq!(got["applied"], json!(["Hall"]));
    assert_eq!(got["undo_depth"], json!(4), "apply is not an undo step");
    let err = method("editor.audio_mixer_chain")(&mut world, &json!({ "bus": "World", "op": "spin" }))
        .expect_err("op");
    assert!(format!("{err:?}").contains("audio_mixer_bad_op"), "{err:?}");
    let err = method("editor.audio_mixer_save")(&mut world, &json!({})).expect_err("no project");
    assert!(format!("{err:?}").contains("audio_mixer_no_project"), "{err:?}");
}

fn key(key: egui::Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}

/// Draws the dock once with `events` (twice when `double`: egui's second pass of one frame);
/// returns the focused widget's accessible name and checks that every widget has one.
fn draw_dock(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>, double: bool) -> String {
    let mut passes = 0;
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1_200.0, 4_000.0),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            passes += 1;
            egui::CentralPanel::default().show(ui, |ui| audio_mixer_panel(ui, world));
            if double && passes == 1 {
                ui.ctx().request_discard("exercise the second pass");
            }
        },
    );
    output.textures_delta.clear();
    let update = output.platform_output.accesskit_update.expect("accessible tree");
    let unnamed = crate::a11y_audit::unnamed_widgets(&update);
    assert!(unnamed.is_empty(), "unnamed widgets: {unnamed:?}");
    update
        .nodes
        .iter()
        .find(|(id, _)| *id == update.focus)
        .and_then(|(_, node)| node.label())
        .unwrap_or_default()
        .to_owned()
}

/// A fresh context (no focus yet) Tabbed until `name` has the focus.
fn tab_to(world: &mut World, name: &str) -> egui::Context {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    crate::theme::sync(&ctx, world);
    draw_dock(&ctx, world, vec![], false);
    for _ in 0..200 {
        if draw_dock(&ctx, world, vec![key(egui::Key::Tab)], false) == name {
            return ctx;
        }
    }
    panic!("Tab never reached {name}");
}

/// The keyboard path of every new control: Tab reaches the strip's Chain toggle, Add effect,
/// Bypass, Save snapshot and Apply; Space operates each; a second egui pass of the same frame
/// acts once.
#[test]
fn the_chain_and_snapshot_controls_work_from_the_keyboard() {
    let mut world = world_with_audio();
    let ctx = tab_to(&mut world, "Chain");
    draw_dock(&ctx, &mut world, vec![key(egui::Key::Space)], false);
    assert_eq!(
        world.resource::<AudioMixerPanel>().open_strip.as_deref(),
        Some("Master"),
        "Space opens the first strip's chain"
    );
    let ctx = tab_to(&mut world, "Add effect");
    draw_dock(&ctx, &mut world, vec![key(egui::Key::Space)], true);
    let master = |world: &World| {
        world
            .resource::<AudioMixerPanel>()
            .bus("Master")
            .expect("master")
            .clone()
    };
    assert_eq!(
        master(&world).effects.len(),
        1,
        "one EQ, not two: the second pass is ignored"
    );
    assert_eq!(master(&world).effects[0].kind(), "eq");
    let ctx = tab_to(&mut world, "Bypass");
    draw_dock(&ctx, &mut world, vec![key(egui::Key::Space)], false);
    assert!(master(&world).effects[0].is_bypassed(), "Space bypasses it");
    world.resource_mut::<AudioMixerPanel>().new_snapshot = "Calm".into();
    let ctx = tab_to(&mut world, "Save snapshot");
    draw_dock(&ctx, &mut world, vec![key(egui::Key::Enter)], false);
    assert_eq!(world.resource::<AudioMixerPanel>().snapshots()[0].name, "Calm");
    let ctx = tab_to(&mut world, "Apply");
    draw_dock(&ctx, &mut world, vec![key(egui::Key::Space)], false);
    pump(&mut world, None);
    assert_eq!(
        world.resource::<AudioMixerPanel>().active_snapshot(),
        Some("Calm")
    );
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 3, "add, bypass, save");
}

// ------------------------------------------------------------------ pre-fader sends (ADR-8590)

/// An app with the agent plugin, the editor's methods and a headless mixer: requests go through
/// the agent dispatcher, one frame each.
struct TapDispatch {
    app: crate::engine::app::App,
    sender: crossbeam_channel::Sender<regia_agent::server::Inbound>,
}

impl TapDispatch {
    fn new() -> Self {
        let root = regia_test_utils::unique_temp_dir("audio-mixer-tap");
        let mut app = crate::engine::app::App::new();
        app.add_plugins(crate::engine::app::MinimalPlugins);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        crate::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.insert_resource(Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        }));
        app.insert_resource(UndoStack::new());
        let (sender, inbox) = crossbeam_channel::unbounded();
        app.insert_resource(regia_agent::AgentInbox(inbox));
        Self { app, sender }
    }

    fn call(&mut self, method: &str, params: Value) -> regia_agent::protocol::Response {
        let (reply, answer) = crossbeam_channel::bounded(1);
        self.sender
            .send(regia_agent::server::Inbound {
                request: regia_agent::protocol::Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("agent inbox");
        self.app.update();
        answer.try_recv().expect("dispatch answers in one frame")
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let response = self.call(method, params);
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.expect("result")
    }

    /// The panel's own error code: `data.reason` (`data.code` is the protocol's F0002 / F0003,
    /// `PanelError::rpc`).
    fn code(&mut self, method: &str, params: Value) -> String {
        let response = self.call(method, params);
        let error = response.error.unwrap_or_else(|| panic!("{method} must fail"));
        error
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// `World`'s send into `Reverb` as the dock state answers it: `(volume_db, tap)`.
    fn world_send(&mut self) -> (f64, String) {
        let state = self.ok("editor.audio_mixer_get", json!({}));
        let bus = state["buses"]
            .as_array()
            .expect("buses")
            .iter()
            .find(|b| b["name"] == "World")
            .expect("World")
            .clone();
        let send = bus["sends"]
            .as_array()
            .expect("sends")
            .iter()
            .find(|s| s["bus"] == "Reverb")
            .expect("the Reverb route")
            .clone();
        (
            send["volume_db"].as_f64().expect("level"),
            send["tap"].as_str().expect("tap").to_owned(),
        )
    }

    /// `World`'s Reverb send tap on the pumped mixer.
    fn mixer_tap(&mut self) -> regia_audio::SendTap {
        pump(self.app.world_mut(), None);
        self.app
            .world()
            .resource::<Audio>()
            .bus_layout()
            .buses
            .iter()
            .find(|b| b.name == "World")
            .and_then(|b| b.sends.iter().find(|s| s.bus == "Reverb"))
            .expect("the mixer's World → Reverb route")
            .tap
    }
}

#[test]
fn the_send_tap_toggles_through_the_dispatcher_and_undoes_in_one_step() {
    use regia_audio::SendTap;
    let mut d = TapDispatch::new();
    assert_eq!(
        d.world_send(),
        (-60.0, "post_fader".to_owned()),
        "the default tree's route"
    );
    assert_eq!(d.mixer_tap(), SendTap::PostFader);

    let got = d.ok(
        "editor.audio_mixer_send",
        json!({ "bus": "World", "send": "Reverb", "tap": "pre_fader" }),
    );
    assert_eq!(got["changed"], json!(true));
    assert_eq!(got["undo_depth"], json!(1), "the toggle is one undo step");
    assert_eq!(
        d.world_send(),
        (-60.0, "pre_fader".to_owned()),
        "the level is kept"
    );
    assert_eq!(d.mixer_tap(), SendTap::PreFader, "the pumped mixer takes the tap");

    // Tap and level in one request: one step. The same tap again changes nothing.
    let got = d.ok(
        "editor.audio_mixer_send",
        json!({ "bus": "World", "send": "Reverb", "tap": "pre_fader", "volume_db": -6 }),
    );
    assert_eq!(got["undo_depth"], json!(2));
    assert_eq!(d.world_send(), (-6.0, "pre_fader".to_owned()));
    let got = d.ok(
        "editor.audio_mixer_send",
        json!({ "bus": "World", "send": "Reverb", "tap": "pre_fader" }),
    );
    assert_eq!(got["changed"], json!(false));
    assert_eq!(got["undo_depth"], json!(2));

    // A level scrub after the toggle is its own step, never merged into the tap change.
    d.ok(
        "editor.audio_mixer_send",
        json!({ "bus": "World", "send": "Reverb", "volume_db": -3 }),
    );
    assert_eq!(d.world_send(), (-3.0, "pre_fader".to_owned()));
    d.ok("editor.undo", json!({}));
    assert_eq!(d.world_send(), (-6.0, "pre_fader".to_owned()));
    d.ok("editor.undo", json!({}));
    assert_eq!(d.world_send(), (-60.0, "pre_fader".to_owned()));
    d.ok("editor.undo", json!({}));
    assert_eq!(
        d.world_send(),
        (-60.0, "post_fader".to_owned()),
        "undo restores the tap"
    );
    assert_eq!(d.mixer_tap(), SendTap::PostFader);

    // A snapshot stores the send level only; applying it leaves the tap alone.
    d.ok(
        "editor.audio_mixer_send",
        json!({ "bus": "World", "send": "Reverb", "tap": "pre_fader" }),
    );
    let got = d.ok(
        "editor.audio_mixer_snapshot",
        json!({ "op": "save", "name": "Wet", "blend_secs": 0 }),
    );
    let snapshot = &got["snapshots"][0];
    assert!(snapshot.get("tap").is_none(), "{snapshot}");
    d.ok(
        "editor.audio_mixer_send",
        json!({ "bus": "World", "send": "Reverb", "tap": "post_fader" }),
    );
    d.ok(
        "editor.audio_mixer_snapshot",
        json!({ "op": "apply", "name": "Wet" }),
    );
    assert_eq!(
        d.world_send().1,
        "post_fader",
        "the snapshot did not touch the tap"
    );
    assert_eq!(d.mixer_tap(), SendTap::PostFader);

    // A missing route is added, silent, with the tap; a bad tap is refused.
    d.ok(
        "editor.audio_mixer_send",
        json!({ "bus": "Music", "send": "Reverb", "tap": "pre_fader" }),
    );
    let state = d.ok("editor.audio_mixer_get", json!({}));
    let music = state["buses"]
        .as_array()
        .expect("buses")
        .iter()
        .find(|b| b["name"] == "Music")
        .expect("Music")
        .clone();
    assert_eq!(
        music["sends"],
        json!([{ "bus": "Reverb", "volume_db": -60.0, "tap": "pre_fader" }])
    );
    assert_eq!(
        d.code(
            "editor.audio_mixer_send",
            json!({ "bus": "World", "send": "Reverb", "tap": "sideways" })
        ),
        "audio_mixer_bad_value"
    );
    assert_eq!(
        d.code(
            "editor.audio_mixer_send",
            json!({ "bus": "World", "send": "Nope", "tap": "pre_fader" })
        ),
        "audio_mixer_unknown_send"
    );
}

/// The dock's Pre toggle from the keyboard: Tab reaches it on the open strip's send row, Space
/// sets the send pre-fader as one undo step, and a second egui pass of the frame acts once.
#[test]
fn the_pre_toggle_works_from_the_keyboard() {
    let mut world = world_with_audio();
    assert!(edit_send(&mut world, "World", "Reverb", Some(-12.0)).expect("level"));
    world.resource_mut::<AudioMixerPanel>().open_strip = Some("World".into());
    let depth = world.resource::<UndoStack>().undo_depth();
    let ctx = tab_to(&mut world, "Pre");
    draw_dock(&ctx, &mut world, vec![key(egui::Key::Space)], true);
    let send = world
        .resource::<AudioMixerPanel>()
        .bus("World")
        .expect("World")
        .sends
        .iter()
        .find(|s| s.bus == "Reverb")
        .cloned()
        .expect("route");
    assert_eq!(send.tap, regia_audio::SendTap::PreFader, "Space toggles Pre");
    assert_eq!(send.volume_db, -12.0);
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "one step, even with egui's second pass"
    );
    assert!(crate::command::undo_world(&mut world));
    assert_eq!(
        world
            .resource::<AudioMixerPanel>()
            .bus("World")
            .expect("World")
            .sends[0]
            .tap,
        regia_audio::SendTap::PostFader
    );
}

/// Bug 25 (ADR-8586): a silent level reads `−∞ dB`, not `-inf`, and levels carry their unit.
#[test]
fn levels_read_minus_infinity_and_decibels() {
    assert_eq!(db_text(METER_FLOOR_DB), "−∞ dB");
    assert_eq!(db_text(f32::NEG_INFINITY), "−∞ dB");
    assert_eq!(db_text(-12.0), "-12.0 dB");
}
