// SPDX-License-Identifier: MPL-2.0
//! The acceptance of the sequencer timeline (ADR-7322): batch key edits across tracks through the
//! agent handlers and the pointer, one undo entry per gesture, paste into a second sequence,
//! markers through save, reload and CSV, a cut from a camera, the transport, and a 200-track
//! timeline drawn headless within a measured bound.

use regia_framework::sequencer::edit::timeline::still_shot;
use regia_framework::sequencer::{Ease, ScalarKey, Track, Vec3Key};

use super::*;
use crate::agent_ext::EditorProject;
use crate::command::undo_world;

fn code(e: RpcError) -> String {
    e.data
        .and_then(|d| d["code"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn float_track(name: &str, keys: &[(f32, f32)]) -> Track {
    Track {
        name: name.into(),
        target: "ui".into(),
        property: name.into(),
        keys: Channel::Float(
            keys.iter()
                .map(|&(t, v)| ScalarKey::new(t, v, Ease::Bezier))
                .collect(),
        ),
        enabled: true,
        locked: false,
    }
}

fn vec3_track(name: &str, keys: &[(f32, [f32; 3])]) -> Track {
    Track {
        name: name.into(),
        target: "cube".into(),
        property: "transform.position".into(),
        keys: Channel::Vec3(
            keys.iter()
                .map(|&(t, v)| Vec3Key::new(t, v, Ease::Bezier))
                .collect(),
        ),
        enabled: true,
        locked: false,
    }
}

fn three_tracks() -> Sequence {
    Sequence {
        name: "shots".into(),
        fps: 30.0,
        tracks: vec![
            float_track("a", &[(0.0, 0.0), (1.0, 2.0), (2.0, 1.0)]),
            vec3_track(
                "b",
                &[(0.0, [0.0; 3]), (1.5, [1.0, 2.0, 3.0]), (3.0, [0.0, 4.0, 0.0])],
            ),
            float_track("c", &[(0.5, 1.0), (2.5, 3.0)]),
        ],
        ..Sequence::default()
    }
}

fn world_with(sequence: Sequence) -> (World, Entity) {
    let mut world = World::new();
    world.init_resource::<UndoStack>();
    let player = world.spawn(SequencePlayer::new(sequence)).id();
    (world, player)
}

fn sequence(world: &World, player: Entity) -> Sequence {
    world
        .get::<SequencePlayer>(player)
        .expect("player")
        .sequence()
        .clone()
}

fn depth(world: &World) -> usize {
    world.resource::<UndoStack>().undo_depth()
}

/// Acceptance: keys box-selected on three tracks move +0.5 s; every selected key moves +0.5 s,
/// the sampler agrees, and the move is one undo entry.
#[test]
fn box_selected_keys_on_three_tracks_move_half_a_second_as_one_undo_entry() {
    let (mut world, player) = world_with(three_tracks());
    let before = sequence(&world, player);
    let picked = seq_keys(
        &mut world,
        &json!({ "op": "select", "range": [0.4, 2.6], "tracks": [0, 1, 2] }),
    )
    .expect("box");
    let selection = picked["selection"].as_array().expect("list").clone();
    assert_eq!(selection.len(), 5, "1 s, 2 s of a; 1.5 s of b; 0.5 s, 2.5 s of c");
    let moved = seq_keys(&mut world, &json!({ "op": "move", "delta": 0.5 })).expect("move");
    let after = sequence(&world, player);
    for (old, new) in selection.iter().zip(moved["selection"].as_array().expect("list")) {
        let (t0, t1) = (old["time"].as_f64().expect("t"), new["time"].as_f64().expect("t"));
        assert_eq!(t1, t0 + 0.5, "{old} -> {new}");
    }
    // The sampler agrees: on c (both keys moved) the whole curve shifted.
    for step in 0..=12 {
        let t = 0.5 + step as f32 * 0.25;
        assert_eq!(
            after.tracks[2].keys.sample_component(t + 0.5, 0),
            before.tracks[2].keys.sample_component(t, 0),
            "c at {t}"
        );
    }
    assert_eq!(
        after.tracks[1].keys.sample(2.0),
        before.tracks[1].keys.sample(1.5),
        "b's moved key"
    );
    assert_eq!(depth(&world), 1, "one undo entry");
    assert!(undo_world(&mut world));
    assert_eq!(sequence(&world, player), before, "undo puts every key back");
}

/// Acceptance: scale x2 about the playhead (one undo entry); a refused scale changes nothing.
#[test]
fn keys_scale_twice_about_the_playhead() {
    let (mut world, player) = world_with(three_tracks());
    seq_transport(&mut world, &json!({ "op": "seek", "time": 1.0 })).expect("seek");
    let before = sequence(&world, player);
    seq_keys(&mut world, &json!({ "op": "select", "keys": [{ "track": "a", "index": 1 }, { "track": "a", "index": 2 }, { "track": 2, "index": 1 }] }))
        .expect("select");
    let scaled = seq_keys(&mut world, &json!({ "op": "scale", "factor": 2.0 })).expect("scale");
    let times: Vec<f64> = scaled["selection"]
        .as_array()
        .expect("list")
        .iter()
        .map(|k| k["time"].as_f64().expect("t"))
        .collect();
    assert_eq!(times, vec![1.0, 3.0, 4.0], "1 + (t - 1) * 2 for 1, 2 and 2.5");
    assert_eq!(depth(&world), 1);
    // The key at 0.5 s would go to 0 s and the one at 0 s to -1 s: refused, nothing changed.
    let keep = sequence(&world, player);
    let err = seq_keys(
        &mut world,
        &json!({ "op": "scale", "factor": 2.0, "keys": [{ "track": 0, "index": 0 }] }),
    )
    .expect_err("negative");
    assert_eq!(code(err), "seq_keys_out_of_range");
    assert_eq!(sequence(&world, player), keep);
    assert_eq!(depth(&world), 1, "a refused edit is no undo entry");
    assert!(undo_world(&mut world));
    assert_eq!(sequence(&world, player), before);
}

/// Acceptance: copied keys paste at the playhead of a second sequence.
#[test]
fn copied_keys_paste_into_a_second_sequence() {
    let (mut world, first) = world_with(three_tracks());
    let second = world
        .spawn(SequencePlayer::new(Sequence {
            tracks: vec![float_track("a", &[(0.0, 9.0)])],
            ..Sequence::default()
        }))
        .id();
    let copied = seq_keys(
        &mut world,
        &json!({
            "op": "copy",
            "player": first.to_bits(),
            "keys": [{ "track": 0, "index": 1 }, { "track": 0, "index": 2 }],
        }),
    )
    .expect("copy");
    assert_eq!(copied["clipboard"], json!(2));
    seq_transport(
        &mut world,
        &json!({ "op": "seek", "time": 0.0, "player": second.to_bits() }),
    )
    .expect("seek");
    let pasted = seq_keys(
        &mut world,
        &json!({ "op": "paste", "time": 3.0, "player": second.to_bits() }),
    );
    let pasted = pasted.expect("paste");
    assert_eq!(pasted["selection"].as_array().map(Vec::len), Some(2));
    let keys = key_times(&sequence(&world, second), 0);
    assert_eq!(keys, Some(vec![0.0, 3.0, 4.0]));
    assert_eq!(
        sequence(&world, second).tracks[0].keys.sample_component(3.0, 0),
        Some(2.0)
    );
    assert_eq!(sequence(&world, first), three_tracks(), "the source is unchanged");
    assert!(undo_world(&mut world));
    assert_eq!(key_times(&sequence(&world, second), 0), Some(vec![0.0]));
    // Pasting into a player without a matching track is refused with a code.
    let third = world
        .spawn(SequencePlayer::new(Sequence {
            tracks: vec![float_track("zzz", &[(0.0, 0.0)])],
            ..Sequence::default()
        }))
        .id();
    let err =
        seq_keys(&mut world, &json!({ "op": "paste", "player": third.to_bits() })).expect_err("no track");
    assert_eq!(code(err), "seq_keys_no_track");
}

/// Acceptance: marker add, move, rename, payload and delete round-trip through save and reload;
/// a CSV export then import is byte-equal; each edit is one undo entry.
#[test]
fn markers_round_trip_through_save_reload_and_csv() {
    let root = regia_test_utils::unique_temp_dir("seq_timeline_markers");
    std::fs::create_dir_all(root.join("cine")).expect("project dir");
    let (mut world, _) = world_with(three_tracks());
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    seq_transport(&mut world, &json!({ "op": "seek", "time": 0.5 })).expect("seek");
    let added = seq_marker_edit(&mut world, &json!({ "op": "add" })).expect("add");
    assert_eq!(
        added["markers"][0],
        json!({ "name": "marker1", "time": 0.5, "payload": "" })
    );
    seq_marker_edit(&mut world, &json!({ "op": "add", "name": "gone", "time": 2.0 })).expect("add");
    seq_marker_edit(
        &mut world,
        &json!({ "op": "move", "marker": "marker1", "time": 1.25 }),
    )
    .expect("move");
    seq_marker_edit(
        &mut world,
        &json!({ "op": "rename", "marker": 0, "name": "door" }),
    )
    .expect("rename");
    seq_marker_edit(
        &mut world,
        &json!({ "op": "payload", "marker": "door", "payload": "play, \"creak\"" }),
    )
    .expect("payload");
    seq_marker_edit(&mut world, &json!({ "op": "delete", "marker": "gone" })).expect("delete");
    assert_eq!(depth(&world), 6, "one undo entry per edit");
    let err = seq_marker_edit(
        &mut world,
        &json!({ "op": "move", "marker": "door", "time": -1.0 }),
    )
    .expect_err("neg");
    assert_eq!(code(err), "seq_marker_invalid");
    let err = seq_marker_edit(
        &mut world,
        &json!({ "op": "rename", "marker": "nope", "name": "x" }),
    )
    .expect_err("nope");
    assert_eq!(code(err), "seq_marker_bad_index");
    // Save, reopen: the markers are what was edited.
    let call = |world: &mut World, name: &str, params: Value| {
        crate::seq_curve_panel::methods()
            .into_iter()
            .find(|(m, _)| m.name == name)
            .map(|(_, h)| h)
            .expect("method")(world, &params)
    };
    call(
        &mut world,
        "editor.seq_curve_save",
        json!({ "path": "cine/a.seq.ron" }),
    )
    .expect("save");
    let opened = call(
        &mut world,
        "editor.seq_curve_open",
        json!({ "path": "cine/a.seq.ron" }),
    )
    .expect("open");
    let reopened = opened["player"]
        .as_u64()
        .and_then(Entity::try_from_bits)
        .expect("player");
    let markers = sequence(&world, reopened).markers;
    assert_eq!(
        markers,
        vec![Marker {
            name: "door".into(),
            time: 1.25,
            payload: "play, \"creak\"".into(),
        }]
    );
    // Export to a file, import it into the reopened player, export again: byte-equal.
    let exported = seq_marker_edit(
        &mut world,
        &json!({ "op": "export_csv", "path": "cine/markers.csv" }),
    )
    .expect("export");
    let csv = exported["csv"].as_str().expect("text").to_string();
    assert_eq!(
        std::fs::read_to_string(root.join("cine/markers.csv")).ok(),
        Some(csv.clone())
    );
    seq_marker_edit(&mut world, &json!({ "op": "delete", "marker": "door" })).expect("delete");
    seq_marker_edit(
        &mut world,
        &json!({ "op": "import_csv", "path": "cine/markers.csv" }),
    )
    .expect("import");
    let again = seq_marker_edit(&mut world, &json!({ "op": "export_csv" })).expect("export");
    assert_eq!(again["csv"].as_str(), Some(csv.as_str()), "byte-equal");
    let err =
        seq_marker_edit(&mut world, &json!({ "op": "import_csv", "path": "../x.csv" })).expect_err("path");
    assert_eq!(code(err), "seq_marker_bad_path");
    let _ = std::fs::remove_dir_all(&root);
}

/// A cut at the playhead from a camera, on a player without a shot list, then on its own list;
/// one undo entry each.
#[test]
fn a_cut_from_the_camera_goes_into_the_players_shot_list() {
    let (mut world, player) = world_with(three_tracks());
    let camera = world
        .spawn((
            Transform::from_translation(regia_core::math::Vec3::new(4.0, 2.0, 0.0)),
            Camera::default(),
        ))
        .id();
    seq_transport(&mut world, &json!({ "op": "seek", "time": 1.0 })).expect("seek");
    let made = seq_cut_edit(&mut world, &json!({ "op": "add", "camera": camera.to_bits() })).expect("add");
    assert_eq!(made["cuts"]["host"], json!("player"));
    assert_eq!(made["cuts"]["cuts"], json!([1.0]));
    let shots = world
        .get::<SequencePlayer>(player)
        .and_then(|p| p.shots().cloned())
        .expect("a list");
    assert_eq!(shots.duration(), 3.0, "as long as the sequence");
    assert_eq!(shots.sample(1.0).map(|s| s.eye.to_array()), Some([4.0, 2.0, 0.0]));
    // The active camera (no `camera`, nothing selected) cuts again at 2 s; fade the new shot.
    seq_cut_edit(&mut world, &json!({ "op": "add", "time": 2.0 })).expect("add");
    let faded = seq_cut_edit(&mut world, &json!({ "op": "fade", "shot": 2, "fade_out": 0.5 })).expect("fade");
    assert_eq!(faded["cuts"]["shots"][2]["fade_out"], json!(0.5));
    let moved = seq_cut_edit(&mut world, &json!({ "op": "move", "cut": 0, "time": 0.5 })).expect("move");
    assert_eq!(moved["cuts"]["cuts"], json!([0.5, 2.0]));
    let err = seq_cut_edit(&mut world, &json!({ "op": "move", "cut": 0, "time": 2.5 })).expect_err("past");
    assert_eq!(code(err), "seq_cut_out_of_range");
    assert_eq!(depth(&world), 4);
    for _ in 0..4 {
        assert!(undo_world(&mut world));
    }
    assert!(
        world
            .get::<SequencePlayer>(player)
            .is_some_and(|p| p.shots().is_none()),
        "undo removes the list it made"
    );
}

#[test]
fn the_transport_plays_loops_its_range_steps_frames_and_shows_timecode() {
    let (mut world, player) = world_with(three_tracks());
    let time = |world: &World| world.get::<SequencePlayer>(player).expect("p").time();
    seq_transport(&mut world, &json!({ "op": "play" })).expect("play");
    seq_transport(&mut world, &json!({ "op": "tick", "dt": 0.5 })).expect("tick");
    assert_eq!(time(&world), 0.5);
    seq_transport(&mut world, &json!({ "op": "range", "in": 1.0, "out": 2.0 })).expect("range");
    seq_transport(&mut world, &json!({ "op": "loop", "on": true })).expect("loop");
    seq_transport(&mut world, &json!({ "op": "seek", "time": 1.75 })).expect("seek");
    seq_transport(&mut world, &json!({ "op": "tick", "dt": 0.5 })).expect("tick");
    assert_eq!(time(&world), 1.25, "wrapped inside the range");
    seq_transport(&mut world, &json!({ "op": "reverse" })).expect("reverse");
    seq_transport(&mut world, &json!({ "op": "tick", "dt": 0.5 })).expect("tick");
    assert_eq!(
        time(&world),
        1.75,
        "reverse wraps from the in point to the out point"
    );
    seq_transport(&mut world, &json!({ "op": "loop", "on": false })).expect("loop off");
    seq_transport(&mut world, &json!({ "op": "play" })).expect("play");
    let out = seq_transport(&mut world, &json!({ "op": "tick", "dt": 1.0 })).expect("tick");
    assert_eq!(out["time"], json!(2.0), "stops at the out point");
    assert_eq!(out["transport"]["playing"], json!(false));
    let step = seq_transport(&mut world, &json!({ "op": "step", "frames": -3 })).expect("step");
    assert_eq!(step["frame"], json!(57), "three frames back at 30 fps");
    assert_eq!(step["timecode"], json!("00:00:01:27"));
    seq_transport(&mut world, &json!({ "op": "display", "mode": "frames" })).expect("display");
    let home = seq_transport(&mut world, &json!({ "op": "home" })).expect("home");
    assert_eq!(home["time"], json!(1.0), "the range's start");
    let err = seq_transport(&mut world, &json!({ "op": "range", "in": 2.0, "out": 1.0 })).expect_err("bad");
    assert_eq!(code(err), "seq_transport_bad_range");
    seq_transport(&mut world, &json!({ "op": "range" })).expect("clear");
    let end = seq_transport(&mut world, &json!({ "op": "end" })).expect("end");
    assert_eq!(end["time"], json!(3.0), "the sequence's end without a range");
    let snap = seq_transport(&mut world, &json!({ "op": "snap", "on": false })).expect("snap");
    assert_eq!(snap["op"], json!("snap"));
    seq_transport(&mut world, &json!({ "op": "follow", "on": false })).expect("follow");
    assert!(!world.resource::<SeqTimeline>().snap);
    assert!(!world.resource::<SeqTimeline>().transport.follow);
    assert_eq!(depth(&world), 0, "the transport is never an undo entry");
    let state = seq_timeline_state(&mut world, &json!({})).expect("state");
    assert_eq!(state["rows"].as_array().map(Vec::len), Some(3));
    assert_eq!(state["transport"]["display"], json!("frames"));
    assert_eq!(
        state["transport"]["range"],
        json!(null),
        "the range is panel state only"
    );
    assert!(!sequence(&world, player).to_ron().expect("ron").contains("range"));
}

fn frame(
    ctx: &egui::Context,
    world: &mut World,
    events: Vec<egui::Event>,
    modifiers: egui::Modifiers,
) -> usize {
    let raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1000.0, 700.0),
        )),
        events: std::iter::once(egui::Event::ModifiersChanged(modifiers))
            .chain(events)
            .collect(),
        ..Default::default()
    };
    let mut output = ctx.run_ui(raw, |ui| {
        egui::CentralPanel::default().show(ui, |ui| seq_timeline_panel(ui, world));
    });
    output.textures_delta.clear();
    output.shapes.len()
}

fn key_point(world: &World, key: KeyRef) -> egui::Pos2 {
    mark_point(world, MarkRef::Key(key))
}

fn mark_point(world: &World, mark: MarkRef) -> egui::Pos2 {
    world
        .resource::<SeqTimeline>()
        .key_points
        .iter()
        .find(|(m, _)| *m == mark)
        .map(|(_, p)| *p)
        .expect("the mark is drawn")
}

/// Shift-clicks build a selection on two rows, a pointer drag of a key moves the selection by
/// whole frames as one undo entry, and a box drag selects across rows.
#[test]
fn pointer_clicks_drags_and_a_box_select_and_move_keys_as_one_undo_entry() {
    let (mut world, player) = world_with(three_tracks());
    let ctx = egui::Context::default();
    let none = egui::Modifiers::NONE;
    frame(&ctx, &mut world, vec![], none);
    frame(&ctx, &mut world, vec![], none);
    let a1 = KeyRef { track: 0, index: 1 };
    let c1 = KeyRef { track: 2, index: 1 };
    let start = key_point(&world, a1);
    let press = |pos: egui::Pos2, pressed: bool, modifiers: egui::Modifiers| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers,
    };
    // Shift-click a1 and c1 into the selection.
    for key in [a1, c1] {
        let p = key_point(&world, key);
        let shift = egui::Modifiers::SHIFT;
        frame(
            &ctx,
            &mut world,
            vec![egui::Event::PointerMoved(p), press(p, true, shift)],
            shift,
        );
        frame(&ctx, &mut world, vec![press(p, false, shift)], shift);
    }
    assert_eq!(
        world.resource::<SeqTimeline>().selection,
        vec![MarkRef::Key(a1), MarkRef::Key(c1)]
    );
    // Drag a1 right by 50.7 points: 0.507 s at 100 points a second, snapped to 0.5 s (15 frames).
    let to = start + egui::vec2(50.7, 0.0);
    frame(
        &ctx,
        &mut world,
        vec![egui::Event::PointerMoved(start), press(start, true, none)],
        none,
    );
    for x in [10.0, 30.0, 50.7] {
        frame(
            &ctx,
            &mut world,
            vec![egui::Event::PointerMoved(start + egui::vec2(x, 0.0))],
            none,
        );
    }
    frame(&ctx, &mut world, vec![press(to, false, none)], none);
    let moved = sequence(&world, player);
    assert_eq!(key_times(&moved, 0), Some(vec![0.0, 1.5, 2.0]));
    assert_eq!(key_times(&moved, 2), Some(vec![0.5, 3.0]));
    assert_eq!(depth(&world), 1, "the whole drag is one undo entry");
    assert!(undo_world(&mut world));
    assert_eq!(sequence(&world, player), three_tracks());
    // A box drag from empty space selects every key it covers.
    frame(&ctx, &mut world, vec![], none);
    let a2 = key_point(&world, KeyRef { track: 0, index: 2 });
    let c0 = key_point(&world, KeyRef { track: 2, index: 0 });
    let from = egui::pos2(c0.x - 8.0, a2.y - 6.0);
    let corner = egui::pos2(a2.x + 8.0, c0.y + 6.0);
    frame(
        &ctx,
        &mut world,
        vec![egui::Event::PointerMoved(from), press(from, true, none)],
        none,
    );
    frame(&ctx, &mut world, vec![egui::Event::PointerMoved(corner)], none);
    frame(&ctx, &mut world, vec![press(corner, false, none)], none);
    let picked = world.resource::<SeqTimeline>().selection.clone();
    assert_eq!(
        picked,
        vec![
            MarkRef::Key(KeyRef { track: 0, index: 1 }),
            MarkRef::Key(KeyRef { track: 0, index: 2 }),
            MarkRef::Key(KeyRef { track: 1, index: 1 }),
            MarkRef::Key(KeyRef { track: 2, index: 0 }),
        ],
        "keys from 0.5 s to 2 s on the three rows"
    );
}

/// Acceptance: a 200-track timeline draws headless with a bounded amount of work: only the rows
/// in view are laid out and painted, so the shape count does not grow with the track count.
#[test]
fn a_200_track_timeline_draws_only_the_rows_in_view() {
    let many = |n: usize| Sequence {
        fps: 30.0,
        tracks: (0..n)
            .map(|i| {
                let keys: Vec<(f32, f32)> = (0..20).map(|k| (k as f32 * 0.25, (i + k) as f32)).collect();
                float_track(&format!("t{i:03}"), &keys)
            })
            .collect(),
        ..Sequence::default()
    };
    let ctx = egui::Context::default();
    let measure = |n: usize| {
        let (mut world, _) = world_with(many(n));
        frame(&ctx, &mut world, vec![], egui::Modifiers::NONE);
        let shapes = frame(&ctx, &mut world, vec![], egui::Modifiers::NONE);
        (world.resource::<SeqTimeline>().rows_drawn, shapes)
    };
    // 60 tracks already overflow the view; 200 must cost the same.
    let (rows_60, shapes_60) = measure(60);
    let (rows_200, shapes_200) = measure(200);
    eprintln!(
        "seq_timeline draw: 60 tracks -> {rows_60} rows, {shapes_60} shapes; 200 tracks -> {rows_200} rows, {shapes_200} shapes"
    );
    assert!(rows_60 < 60, "60 tracks overflow the view ({rows_60} rows drawn)");
    assert_eq!(
        rows_200, rows_60,
        "virtualized: the rows drawn are what fits the view"
    );
    // Measured bound (ADR-7322): 200 tracks paint what 60 do, within the ruler's few labels.
    assert!(
        shapes_200 <= shapes_60 + 16,
        "{shapes_200} shapes for 200 tracks vs {shapes_60} for 60"
    );
}

/// A player sequence with keys and markers past a shot list's cuts, plus that shot list, for the
/// ripple acceptance.
fn ripple_world() -> (World, Entity) {
    let mut sequence = three_tracks();
    sequence.tracks[0].keys = Channel::Float(vec![
        ScalarKey::new(0.5, 0.0, Ease::Bezier),
        ScalarKey::new(1.0, 2.0, Ease::Bezier),
        ScalarKey::new(4.5, 1.0, Ease::Bezier),
        ScalarKey::new(5.5, 3.0, Ease::Bezier),
    ]);
    sequence.markers = vec![
        Marker {
            name: "stays".into(),
            time: 3.4,
            payload: String::new(),
        },
        Marker {
            name: "late".into(),
            time: 5.0,
            payload: String::new(),
        },
    ];
    let (world, player) = world_with(sequence);
    (world, player)
}

fn ripple_shots(world: &mut World, player: Entity) {
    let pose = CutPose {
        position: [0.0, 1.0, 0.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        fov_degrees: 50.0,
    };
    let shots = regia_framework::shots::ShotSequence {
        shots: vec![
            still_shot("one", pose, 2.0),
            still_shot("two", pose, 2.0),
            still_shot("three", pose, 2.0),
        ],
        ..regia_framework::shots::ShotSequence::default()
    };
    if let Some(mut p) = world.get_mut::<SequencePlayer>(player) {
        p.set_shots(Some(shots));
    }
}

/// Acceptance (ripple edit): a rippling trim through the agent handler shifts the sequence's
/// later keys and markers and the shot list's later cut as one undo entry, and one undo restores
/// both. Without `ripple` the keys and markers do not move.
#[test]
fn a_ripple_trim_is_one_undo_entry_that_restores_the_sequence_and_the_shots() {
    let (mut world, player) = ripple_world();
    ripple_shots(&mut world, player);
    let (seq_before, shots_before) = (
        sequence(&world, player),
        world
            .get::<SequencePlayer>(player)
            .and_then(|p| p.shots().cloned())
            .expect("shots"),
    );
    let trimmed = seq_cut_edit(
        &mut world,
        &json!({ "op": "trim", "shot": 1, "duration": 1.5, "ripple": true }),
    )
    .expect("trim");
    assert_eq!(trimmed["cuts"]["cuts"], json!([2.0, 3.5]));
    assert_eq!(
        key_times(&sequence(&world, player), 0),
        Some(vec![0.5, 1.0, 4.0, 5.0])
    );
    assert_eq!(
        sequence(&world, player)
            .markers
            .iter()
            .map(|m| (m.name.as_str(), m.time))
            .collect::<Vec<_>>(),
        vec![("stays", 3.4), ("late", 4.5)],
    );
    assert_eq!(depth(&world), 1, "one undo entry for the whole ripple");
    assert!(undo_world(&mut world));
    assert_eq!(
        sequence(&world, player),
        seq_before,
        "undo restores the keys and markers"
    );
    assert_eq!(
        world
            .get::<SequencePlayer>(player)
            .and_then(|p| p.shots().cloned())
            .expect("shots"),
        shots_before,
        "undo restores the shot list"
    );
    // Without ripple (the base behavior): the later cut moves, the keys and markers do not.
    seq_cut_edit(&mut world, &json!({ "op": "trim", "shot": 1, "duration": 1.5 })).expect("trim");
    assert_eq!(
        key_times(&sequence(&world, player), 0),
        Some(vec![0.5, 1.0, 4.5, 5.5])
    );
    assert_eq!(sequence(&world, player).markers, seq_before.markers);
}

/// Acceptance (ripple edit): a rippling cut move keeps the second shot's clock, so the later cut
/// moves with it, and a rippling move that a refused key shift would break changes nothing.
#[test]
fn a_ripple_cut_move_shifts_the_later_cut_and_the_keys() {
    let (mut world, player) = ripple_world();
    ripple_shots(&mut world, player);
    let moved = seq_cut_edit(
        &mut world,
        &json!({ "op": "move", "cut": 0, "time": 2.5, "ripple": true }),
    )
    .expect("move");
    assert_eq!(moved["cuts"]["cuts"], json!([2.5, 4.5]), "both cuts move +0.5 s");
    assert_eq!(
        key_times(&sequence(&world, player), 0),
        Some(vec![0.5, 1.0, 5.0, 6.0])
    );
    assert_eq!(depth(&world), 1);
    assert!(undo_world(&mut world));
    assert_eq!(
        key_times(&sequence(&world, player), 0),
        Some(vec![0.5, 1.0, 4.5, 5.5])
    );
    // A ripple whose key shift would move a key below zero is refused; nothing changes at all.
    let (mut world, player) = world_with(Sequence {
        tracks: vec![float_track("a", &[(0.2, 0.0), (0.7, 1.0)])],
        ..Sequence::default()
    });
    ripple_shots(&mut world, player);
    // The shots are 2 + 2 + 2 s; shorten the first to 0.5 s so the cut sits at 0.5 s.
    seq_cut_edit(&mut world, &json!({ "op": "move", "cut": 0, "time": 0.5 })).expect("cut to 0.5 s");
    let keep = sequence(&world, player);
    let keep_shots = world
        .get::<SequencePlayer>(player)
        .and_then(|p| p.shots().cloned())
        .expect("shots");
    // Shot 1 (0.5..4.0 s) trimmed to 0.1 s: the 0.7 s key sits in the span the trim removes
    // (0.6..4.0 s), so the ripple is refused rather than reorder it (w9 review finding 3).
    let err = seq_cut_edit(
        &mut world,
        &json!({ "op": "trim", "shot": 1, "duration": 0.1, "ripple": true }),
    )
    .expect_err("the 0.7 s key is in the removed span");
    assert_eq!(code(err), "seq_ripple_keys_in_span");
    assert_eq!(
        sequence(&world, player),
        keep,
        "the refused ripple changed nothing"
    );
    assert_eq!(
        world
            .get::<SequencePlayer>(player)
            .and_then(|p| p.shots().cloned()),
        Some(keep_shots)
    );
    assert_eq!(depth(&world), 1, "the refused ripple is no undo entry");
}

/// A player sequence with a keyed track and a clip section track, for the dope-sheet rows.
fn sections_world() -> (World, Entity) {
    let mut sequence = three_tracks();
    sequence.tracks.push(Track {
        name: "anim".into(),
        target: "rig".into(),
        property: String::new(),
        keys: Channel::Clip(vec![regia_framework::sequencer::ClipSection {
            clip: "walk".into(),
            start: 1.0,
            end: 3.0,
            offset: 0.0,
            speed: 1.0,
            looping: false,
            blend_in: 0.0,
            blend_out: 0.0,
            weight: 1.0,
        }]),
        enabled: true,
        locked: false,
    });
    world_with(sequence)
}

/// Acceptance (section rows): the state lists the section track as a row with its spans; a
/// box-select over a key row and a section row picks the key and the section's start; a pointer
/// drag moves both +0.5 s as one undo entry that undo restores.
#[test]
fn a_box_select_and_drag_moves_a_key_and_a_section_start_together() {
    let (mut world, player) = sections_world();
    let ctx = egui::Context::default();
    let none = egui::Modifiers::NONE;
    frame(&ctx, &mut world, vec![], none);
    frame(&ctx, &mut world, vec![], none);
    // The section track is a row whose start and end are marks.
    let state = seq_timeline_state(&mut world, &json!({})).expect("state");
    let rows = state["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 4, "three keyed tracks and the clip track");
    assert_eq!(rows[3]["kind"], json!("Clip"));
    assert_eq!(
        rows[3]["sections"],
        json!([{ "index": 0, "start": 1.0, "end": 3.0, "label": "walk" }]),
    );
    // Box-select the 1.5 s key of `b` (row 1) and the walk start at 1 s (row 3): the box spans
    // those rows and only those times, so it picks one key mark and one section mark.
    let b1 = mark_point(&world, MarkRef::Key(KeyRef { track: 1, index: 1 }));
    let walk_start = mark_point(
        &world,
        MarkRef::SectionEnd {
            track: 3,
            index: 0,
            end: false,
        },
    );
    let from = egui::pos2(walk_start.x - 4.0, b1.y - 6.0);
    let corner = egui::pos2(b1.x + 4.0, walk_start.y + 6.0);
    let press = |pos: egui::Pos2, pressed: bool, modifiers: egui::Modifiers| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers,
    };
    frame(
        &ctx,
        &mut world,
        vec![egui::Event::PointerMoved(from), press(from, true, none)],
        none,
    );
    frame(&ctx, &mut world, vec![egui::Event::PointerMoved(corner)], none);
    frame(&ctx, &mut world, vec![press(corner, false, none)], none);
    assert_eq!(
        world.resource::<SeqTimeline>().selection,
        vec![
            MarkRef::Key(KeyRef { track: 1, index: 1 }),
            MarkRef::SectionEnd {
                track: 3,
                index: 0,
                end: false
            },
        ],
        "the key and the section start are marks of one selection"
    );
    // Drag the key +0.5 s: both marks move (the section start trims).
    let before = sequence(&world, player);
    let start = mark_point(&world, MarkRef::Key(KeyRef { track: 1, index: 1 }));
    let to = start + egui::vec2(50.7, 0.0);
    frame(
        &ctx,
        &mut world,
        vec![egui::Event::PointerMoved(start), press(start, true, none)],
        none,
    );
    for x in [10.0, 50.7] {
        frame(
            &ctx,
            &mut world,
            vec![egui::Event::PointerMoved(start + egui::vec2(x, 0.0))],
            none,
        );
    }
    frame(&ctx, &mut world, vec![press(to, false, none)], none);
    let moved = sequence(&world, player);
    // Track `b` keys 0, 1.5 and 3 s: only the selected 1.5 s key moves (w9 review finding 9).
    assert_eq!(
        key_times(&moved, 1),
        Some(vec![0.0, 2.0, 3.0]),
        "the key moved +0.5 s"
    );
    let Channel::Clip(sections) = &moved.tracks[3].keys else {
        unreachable!()
    };
    assert_eq!(
        (sections[0].start, sections[0].end),
        (1.5, 3.0),
        "the section start moved +0.5 s"
    );
    assert_eq!(depth(&world), 1, "the whole drag is one undo entry");
    assert!(undo_world(&mut world));
    assert_eq!(sequence(&world, player), before, "undo puts both marks back");
}

/// Acceptance (section rows): a move that would overlap two sections is refused with a code and
/// changes nothing, through the agent handler.
#[test]
fn an_overlapping_section_move_is_refused_with_a_code() {
    let (mut world, player) = sections_world();
    // Two sections: walk 1..3, run 4..6.
    let mut seq = sequence(&world, player);
    if let Channel::Clip(sections) = &mut seq.tracks[3].keys {
        sections.push(regia_framework::sequencer::ClipSection {
            clip: "run".into(),
            start: 4.0,
            end: 6.0,
            offset: 0.0,
            speed: 1.0,
            looping: false,
            blend_in: 0.0,
            blend_out: 0.0,
            weight: 1.0,
        });
    }
    if let Some(mut p) = world.get_mut::<SequencePlayer>(player) {
        p.replace_sequence(seq);
    }
    let keep = sequence(&world, player);
    // Move the whole walk section (both ends) onto the run section.
    let err = seq_keys(
        &mut world,
        &json!({
            "op": "move",
            "delta": 1.5,
            "keys": [
                { "track": "anim", "index": 0, "end": "start" },
                { "track": "anim", "index": 0, "end": "end" },
            ],
        }),
    )
    .expect_err("overlap");
    assert_eq!(code(err), "seq_section_overlap");
    assert_eq!(sequence(&world, player), keep, "the refused move changed nothing");
    assert_eq!(depth(&world), 0);
    // The section refs are documented shapes: a move through them selects the moved marks.
    let moved = seq_keys(
        &mut world,
        &json!({
            "op": "move",
            "delta": 0.5,
            "keys": [{ "track": "anim", "index": 0, "end": "start" }],
        }),
    )
    .expect("move the start");
    assert_eq!(
        moved["selection"],
        json!([{ "track": 3, "name": "anim", "index": 0, "end": "start", "time": 1.5 }]),
    );
}

/// Acceptance (synthetic input): each transport shortcut, pressed while the pointer is over the
/// timeline and no text field has focus, changes the transport exactly as its agent op does on a
/// twin world.
#[test]
fn keyboard_shortcuts_match_their_agent_ops_on_a_twin_world() {
    let key = |key: egui::Key| egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    let transport_of = |world: &World, player: Entity| {
        let timeline = world.resource::<SeqTimeline>();
        let time = world.get::<SequencePlayer>(player).expect("p").time();
        (
            timeline.transport.playing,
            timeline.transport.rate,
            (time * 10_000.0).round() as i64,
        )
    };
    // Each case starts both worlds at the same seek, then presses the key on one and runs the
    // agent op on the other.
    for (seek, key_event, op) in [
        (0.0, key(egui::Key::Space), json!({ "op": "toggle" })),
        (1.0, key(egui::Key::Space), json!({ "op": "toggle" })),
        (0.0, key(egui::Key::J), json!({ "op": "reverse" })),
        (1.0, key(egui::Key::K), json!({ "op": "pause" })),
        (0.0, key(egui::Key::L), json!({ "op": "play" })),
        (1.0, key(egui::Key::Home), json!({ "op": "home" })),
        (1.0, key(egui::Key::End), json!({ "op": "end" })),
        (1.0, key(egui::Key::Comma), json!({ "op": "step", "frames": -1 })),
        (1.0, key(egui::Key::Period), json!({ "op": "step", "frames": 1 })),
    ] {
        let (mut keys_world, keys_player) = world_with(three_tracks());
        let (mut twin_world, twin_player) = world_with(three_tracks());
        for world in [&mut keys_world, &mut twin_world] {
            let _ = seq_transport(world, &json!({ "op": "seek", "time": seek }));
        }
        let ctx = egui::Context::default();
        frame(&ctx, &mut keys_world, vec![], egui::Modifiers::NONE);
        frame(&ctx, &mut keys_world, vec![], egui::Modifiers::NONE);
        let hover = key_point(&keys_world, KeyRef { track: 0, index: 1 });
        frame(
            &ctx,
            &mut keys_world,
            vec![egui::Event::PointerMoved(hover), key_event],
            egui::Modifiers::NONE,
        );
        seq_transport(&mut twin_world, &op).expect("agent op");
        assert_eq!(
            transport_of(&keys_world, keys_player),
            transport_of(&twin_world, twin_player),
            "key {op} from {seek} s"
        );
    }
}

/// Acceptance (synthetic input): the Delete, copy and paste shortcuts do to the keys what
/// `editor.seq_keys` does on a twin world.
#[test]
fn delete_copy_and_paste_shortcuts_match_their_agent_ops() {
    // Delete: both worlds select the 1 s key of `a`, one presses Delete.
    let (mut keys_world, keys_player) = world_with(three_tracks());
    let (mut twin_world, twin_player) = world_with(three_tracks());
    for world in [&mut keys_world, &mut twin_world] {
        seq_keys(
            world,
            &json!({ "op": "select", "keys": [{ "track": "a", "index": 1 }] }),
        )
        .expect("select");
    }
    let ctx = egui::Context::default();
    frame(&ctx, &mut keys_world, vec![], egui::Modifiers::NONE);
    frame(&ctx, &mut keys_world, vec![], egui::Modifiers::NONE);
    let hover = key_point(&keys_world, KeyRef { track: 0, index: 0 });
    frame(
        &ctx,
        &mut keys_world,
        vec![
            egui::Event::PointerMoved(hover),
            egui::Event::Key {
                key: egui::Key::Delete,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
        ],
        egui::Modifiers::NONE,
    );
    seq_keys(&mut twin_world, &json!({ "op": "delete" })).expect("delete");
    assert_eq!(
        sequence(&keys_world, keys_player),
        sequence(&twin_world, twin_player)
    );
    // Copy and paste at the playhead: the shortcuts use the panel's clipboard.
    for world in [&mut keys_world, &mut twin_world] {
        let _ = seq_transport(world, &json!({ "op": "seek", "time": 0.0 }));
        seq_keys(
            world,
            &json!({ "op": "select", "keys": [{ "track": "a", "index": 1 }] }),
        )
        .expect("select");
    }
    frame(&ctx, &mut keys_world, vec![], egui::Modifiers::NONE);
    let hover = key_point(&keys_world, KeyRef { track: 0, index: 0 });
    frame(
        &ctx,
        &mut keys_world,
        vec![egui::Event::PointerMoved(hover), egui::Event::Copy],
        egui::Modifiers::NONE,
    );
    assert_eq!(
        keys_world
            .resource::<SeqTimeline>()
            .clipboard
            .as_ref()
            .map_or(0, KeyClip::len),
        1
    );
    frame(&ctx, &mut keys_world, vec![], egui::Modifiers::NONE);
    frame(
        &ctx,
        &mut keys_world,
        vec![
            egui::Event::PointerMoved(hover),
            egui::Event::Paste(String::new()),
        ],
        egui::Modifiers::NONE,
    );
    seq_keys(&mut twin_world, &json!({ "op": "copy" })).expect("copy");
    seq_keys(&mut twin_world, &json!({ "op": "paste", "time": 0.0 })).expect("paste");
    assert_eq!(
        sequence(&keys_world, keys_player),
        sequence(&twin_world, twin_player)
    );
}

/// Acceptance (synthetic input): with a text field focused, the timeline keys do nothing.
#[test]
fn keyboard_shortcuts_do_nothing_while_a_text_field_has_focus() {
    let (mut world, player) = world_with(three_tracks());
    let ctx = egui::Context::default();
    frame(&ctx, &mut world, vec![], egui::Modifiers::NONE);
    frame(&ctx, &mut world, vec![], egui::Modifiers::NONE);
    // Tab through the focusable widgets until a text edit holds the focus (the marker CSV path
    // field is always drawn).
    let mut focused = false;
    for _ in 0..40 {
        frame(
            &ctx,
            &mut world,
            vec![egui::Event::Key {
                key: egui::Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            egui::Modifiers::NONE,
        );
        if ctx.text_edit_focused() {
            focused = true;
            break;
        }
    }
    assert!(focused, "Tab reached a text field of the timeline");
    let hover = key_point(&world, KeyRef { track: 0, index: 0 });
    let before = world.get::<SequencePlayer>(player).expect("p").time();
    frame(
        &ctx,
        &mut world,
        vec![
            egui::Event::PointerMoved(hover),
            egui::Event::Key {
                key: egui::Key::Space,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
        ],
        egui::Modifiers::NONE,
    );
    assert!(
        !world.resource::<SeqTimeline>().transport.playing,
        "Space did not start the transport"
    );
    assert_eq!(world.get::<SequencePlayer>(player).expect("p").time(), before);
}

/// Acceptance (synthetic input): an Alt-drag on a key scales the selection about the playhead
/// exactly as `editor.seq_keys { op: scale }` does on a twin world.
#[test]
fn an_alt_drag_scales_the_keys_like_the_agent_op() {
    let (mut drag_world, drag_player) = world_with(three_tracks());
    let (mut twin_world, twin_player) = world_with(three_tracks());
    for world in [&mut drag_world, &mut twin_world] {
        seq_transport(world, &json!({ "op": "seek", "time": 1.0 })).expect("seek");
        seq_keys(
            world,
            &json!({ "op": "select", "keys": [{ "track": 2, "index": 1 }] }),
        )
        .expect("select the 2.5 s key of c");
    }
    let ctx = egui::Context::default();
    frame(&ctx, &mut drag_world, vec![], egui::Modifiers::NONE);
    frame(&ctx, &mut drag_world, vec![], egui::Modifiers::NONE);
    let start = key_point(&drag_world, KeyRef { track: 2, index: 1 });
    let alt = egui::Modifiers::ALT;
    let press = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: alt,
    };
    // Drag right by 50 points at 100 points a second: the factor about the 1 s playhead is
    // (3.0 - 1.0) / (2.5 - 1.0).
    let to = start + egui::vec2(50.0, 0.0);
    frame(
        &ctx,
        &mut drag_world,
        vec![egui::Event::PointerMoved(start), press(start, true)],
        alt,
    );
    frame(&ctx, &mut drag_world, vec![egui::Event::PointerMoved(to)], alt);
    frame(&ctx, &mut drag_world, vec![press(to, false)], alt);
    seq_keys(
        &mut twin_world,
        &json!({ "op": "scale", "factor": 4.0 / 3.0, "pivot": 1.0 }),
    )
    .expect("scale");
    assert_eq!(
        sequence(&drag_world, drag_player),
        sequence(&twin_world, twin_player),
        "the Alt-drag scaled like the agent op"
    );
    assert_eq!(depth(&drag_world), 1, "one undo entry");
}

// ---------------------------------------------------------------- w10-seq-timeline-rework

/// w10 item 1 (ADR-8730 on `SeqRippleEdit`): a rippling cut edit on the `ShotPlayer`'s list
/// belongs to the shot-list document it was made on. After another `.shots.ron` is opened, undo
/// of that ripple does nothing: the old list never lands in the new document.
#[test]
fn a_ripple_undo_after_opening_another_shot_list_leaves_that_list_alone() {
    let root = regia_test_utils::unique_temp_dir("seq-ripple-generation");
    std::fs::create_dir_all(root.join("assets")).expect("dir");
    let (a, b) = ("assets/cine/a.shots.ron", "assets/cine/b.shots.ron");
    for rel in [a, b] {
        crate::content::create_asset(&root, crate::content::NewAssetKind::ShotList, rel).expect("shot list");
    }
    let (mut world, player) = world_with(Sequence {
        fps: 30.0,
        tracks: vec![float_track("a", &[(0.5, 0.0), (5.5, 1.0)])],
        ..Sequence::default()
    });
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    world.insert_resource(ShotPlayer::default());
    crate::shot_document::open_shot_list(&mut world, a, false).expect("open a");
    // The player has no list of its own, so the cut row edits the ShotPlayer's (document A).
    let trimmed = seq_cut_edit(
        &mut world,
        &json!({ "op": "trim", "shot": 0, "duration": 4.0, "ripple": true }),
    )
    .expect("ripple trim");
    assert_eq!(trimmed["cuts"]["host"], "shot_player", "{trimmed}");
    assert_eq!(key_times(&sequence(&world, player), 0), Some(vec![0.5, 4.5]));
    assert_eq!(depth(&world), 1);
    // Open B over the edited A (discarding A's edit), then undo the ripple.
    crate::shot_document::open_shot_list(&mut world, b, true).expect("open b");
    let opened = world.resource::<ShotPlayer>().sequence().cloned();
    undo_world(&mut world);
    assert_eq!(
        world.resource::<ShotPlayer>().sequence().cloned(),
        opened,
        "undo of a ripple made on A leaves B alone"
    );
    assert_eq!(crate::shot_document::path(&world).as_deref(), Some(b));
    let _ = std::fs::remove_dir_all(&root);
}

/// A moving shot: its camera path goes from x 0 to x 10 over its duration (linear), so a trim
/// past a key changes the path (the trimmed path ends with a key at the cut).
fn moving_shot(name: &str, duration: f32) -> regia_framework::shots::Shot {
    let pose = CutPose {
        position: [0.0, 1.0, 0.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        fov_degrees: 50.0,
    };
    let mut shot = still_shot(name, pose, duration);
    shot.camera.path.keys = vec![
        regia_framework::shots::PathKey {
            time: 0.0,
            position: [0.0, 1.0, 0.0],
        },
        regia_framework::shots::PathKey {
            time: duration,
            position: [10.0, 1.0, 0.0],
        },
    ];
    shot
}

/// The screen point of time `t` on the cut row (as last drawn).
fn cut_point(world: &World, t: f32) -> egui::Pos2 {
    let timeline = world.resource::<SeqTimeline>();
    let (row, left) = timeline.cut_row.expect("the cut row is drawn");
    egui::pos2(timeline.view.x(left, t), row.center().y)
}

/// w10 item 2 (w9 review finding 3): a rippling cut drag re-applies to the state at the press,
/// so one drag is one ripple whatever the mouse speed. Dragging cut 0 from 2 s to 1.5 s and back
/// to 2 s ends where it started: the first shot's moving path is whole (an incremental drag
/// would have cut it at 1.5 s), the later key is back at 2.5 s, and the drag is one undo entry.
#[test]
fn a_ripple_cut_drag_away_and_back_is_one_ripple_from_the_press() {
    let (mut world, player) = world_with(Sequence {
        fps: 30.0,
        tracks: vec![float_track("a", &[(0.5, 0.0), (2.5, 1.0)])],
        ..Sequence::default()
    });
    let shots = regia_framework::shots::ShotSequence {
        shots: vec![moving_shot("one", 2.0), moving_shot("two", 2.0)],
        ..regia_framework::shots::ShotSequence::default()
    };
    if let Some(mut p) = world.get_mut::<SequencePlayer>(player) {
        p.set_shots(Some(shots.clone()));
    }
    let ctx = egui::Context::default();
    let none = egui::Modifiers::NONE;
    frame(&ctx, &mut world, vec![], none);
    world.resource_mut::<SeqTimeline>().ripple = true;
    frame(&ctx, &mut world, vec![], none);
    let press = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    let at = cut_point(&world, 2.0);
    let left = cut_point(&world, 1.5);
    frame(
        &ctx,
        &mut world,
        vec![egui::Event::PointerMoved(at), press(at, true)],
        none,
    );
    frame(
        &ctx,
        &mut world,
        vec![egui::Event::PointerMoved(at + egui::vec2(-20.0, 0.0))],
        none,
    );
    frame(&ctx, &mut world, vec![egui::Event::PointerMoved(left)], none);
    let mid = world
        .get::<SequencePlayer>(player)
        .and_then(|p| p.shots().cloned())
        .expect("shots");
    assert_eq!(
        regia_framework::sequencer::edit::timeline::cut_times(&mid),
        vec![1.5]
    );
    assert_eq!(key_times(&sequence(&world, player), 0), Some(vec![0.5, 2.0]));
    frame(&ctx, &mut world, vec![egui::Event::PointerMoved(at)], none);
    frame(&ctx, &mut world, vec![press(at, false)], none);
    assert_eq!(
        world
            .get::<SequencePlayer>(player)
            .and_then(|p| p.shots().cloned()),
        Some(shots),
        "back at the press point: the shot list is the one the drag started from"
    );
    assert_eq!(key_times(&sequence(&world, player), 0), Some(vec![0.5, 2.5]));
    assert_eq!(depth(&world), 1, "the whole drag is one undo entry");
}

/// w10 item 8 (w9 review finding 6): a cut edit without ripple is an "Edit sequence cuts" entry
/// with no sequence snapshot, even when the panel has a player; a rippling one is a ripple entry.
#[test]
fn only_a_rippling_cut_edit_records_a_ripple_entry() {
    let (mut world, player) = ripple_world();
    ripple_shots(&mut world, player);
    seq_cut_edit(&mut world, &json!({ "op": "trim", "shot": 1, "duration": 1.5 })).expect("trim");
    let stack = world.resource::<UndoStack>();
    assert_eq!(stack.next_undo().as_deref(), Some("Edit sequence cuts"));
    assert!(
        stack
            .top()
            .is_some_and(|c| downcast_to::<SeqCutsEdit>(c).is_some()),
        "a plain cut edit snapshots the shot list only"
    );
    seq_cut_edit(
        &mut world,
        &json!({ "op": "trim", "shot": 1, "duration": 2.0, "ripple": true }),
    )
    .expect("ripple trim");
    let stack = world.resource::<UndoStack>();
    assert_eq!(stack.next_undo().as_deref(), Some("Ripple sequence cuts"));
}

/// A flat audio track with one `chime` section per span.
fn audio_track(name: &str, spans: &[(f32, f32)]) -> Track {
    Track {
        name: name.into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Audio(
            spans
                .iter()
                .map(|&(a, b)| regia_framework::sequencer::AudioSection::new("chime", a, b))
                .collect(),
        ),
        enabled: true,
        locked: false,
    }
}

/// Section spans of the track named `name`, as `(start, end)` pairs.
fn spans_named(world: &World, player: Entity, name: &str) -> Vec<(f32, f32)> {
    let sequence = sequence(world, player);
    let track = sequence.tracks.iter().find(|t| t.name == name).expect("track");
    regia_framework::sequencer::sections::section_spans(&track.keys)
        .unwrap_or_default()
        .iter()
        .map(|s| (s.start, s.end))
        .collect()
}

/// w11 item 3: a rippling trim through the agent handler moves a later sound section with the
/// keys, in the same undo entry.
#[test]
fn a_ripple_trim_moves_a_later_section_with_the_keys() {
    let (mut world, player) = ripple_world();
    if let Some(mut p) = world.get_mut::<SequencePlayer>(player) {
        let mut s = p.sequence().clone();
        s.tracks.push(audio_track("music", &[(0.5, 1.5), (4.2, 5.2)]));
        p.replace_sequence(s);
    }
    ripple_shots(&mut world, player);
    let before = sequence(&world, player);
    seq_cut_edit(
        &mut world,
        &json!({ "op": "trim", "shot": 1, "duration": 1.5, "ripple": true }),
    )
    .expect("trim");
    let music = spans_named(&world, player, "music");
    assert_eq!(music[0], (0.5, 1.5), "an earlier section stays");
    assert!(
        (music[1].0 - 3.7).abs() < 1.0e-5 && (music[1].1 - 4.7).abs() < 1.0e-5,
        "the later section moves 0.5 s earlier: {music:?}"
    );
    assert_eq!(depth(&world), 1);
    assert!(undo_world(&mut world));
    assert_eq!(sequence(&world, player), before);
}

/// w11 item 3: `editor.seq_keys` op `scale` scales a selection of keys and section ends about the
/// pivot (a section's start and end both scale; its content stays on the clock from its new
/// start), as one undo entry.
#[test]
fn a_scale_through_the_handler_scales_section_spans_with_keys() {
    let (mut world, player) = world_with(Sequence {
        tracks: vec![
            float_track("a", &[(0.0, 0.0), (1.0, 1.0), (3.0, 2.0)]),
            audio_track("music", &[(1.0, 2.0)]),
        ],
        ..Sequence::default()
    });
    let before = sequence(&world, player);
    let out = seq_keys(
        &mut world,
        &json!({ "op": "scale", "factor": 2.0, "pivot": 0.0, "keys": [
            { "track": "a", "index": 1 },
            { "track": "music", "index": 0, "end": "start" },
            { "track": "music", "index": 0, "end": "end" },
        ] }),
    )
    .expect("scale with section ends");
    assert_eq!(key_times(&sequence(&world, player), 0), Some(vec![0.0, 2.0, 3.0]));
    assert_eq!(spans_named(&world, player, "music"), vec![(2.0, 4.0)], "{out}");
    assert_eq!(out["selection"].as_array().map(Vec::len), Some(3), "{out}");
    assert_eq!(depth(&world), 1);
    assert!(undo_world(&mut world));
    assert_eq!(sequence(&world, player), before);
}

/// w11 item 3: `editor.seq_keys` ops `copy` and `paste` carry section spans with keys: a copied
/// section (any of its ends selected) pastes whole at the paste time, relative to the earliest
/// copied mark, into the track of the same name, as one undo entry.
#[test]
fn copy_and_paste_through_the_handler_carry_sections_with_keys() {
    let (mut world, player) = world_with(Sequence {
        tracks: vec![
            float_track("a", &[(0.0, 0.0), (1.0, 1.0), (3.0, 2.0)]),
            audio_track("music", &[(1.0, 2.0)]),
        ],
        ..Sequence::default()
    });
    let copied = seq_keys(
        &mut world,
        &json!({ "op": "copy", "keys": [
            { "track": "a", "index": 1 },
            { "track": "music", "index": 0, "end": "start" },
        ] }),
    )
    .expect("copy with a section end");
    assert_eq!(copied["clipboard"], json!(2), "a key and a section: {copied}");
    let before = sequence(&world, player);
    let pasted = seq_keys(&mut world, &json!({ "op": "paste", "time": 4.0 })).expect("paste");
    assert_eq!(
        key_times(&sequence(&world, player), 0),
        Some(vec![0.0, 1.0, 3.0, 4.0])
    );
    assert_eq!(
        spans_named(&world, player, "music"),
        vec![(1.0, 2.0), (4.0, 5.0)],
        "{pasted}"
    );
    assert_eq!(pasted["selection"].as_array().map(Vec::len), Some(3), "{pasted}");
    assert_eq!(depth(&world), 1);
    assert!(undo_world(&mut world));
    assert_eq!(sequence(&world, player), before);
}

fn event_track(name: &str, keys: &[(f32, &str)]) -> Track {
    Track {
        name: name.into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Event(
            keys.iter()
                .map(|&(t, e)| regia_framework::sequencer::EventKey::new(t, e))
                .collect(),
        ),
        enabled: true,
        locked: false,
    }
}

fn events_named(world: &World, player: Entity, name: &str) -> Vec<(f32, String)> {
    let sequence = sequence(world, player);
    let track = sequence.tracks.iter().find(|t| t.name == name).expect("track");
    match &track.keys {
        Channel::Event(keys) => keys.iter().map(|k| (k.time, k.event.clone())).collect(),
        other => panic!("{name} is a {} track", other.kind()),
    }
}

/// ADR-8628: an event track (sequence v6) is a dope-sheet row in `editor.seq_timeline_state` with
/// its key times; `editor.seq_keys` box-selects its keys, copies them and pastes them at a time
/// (each pasted event after the keys already there, one undo entry), and a rippling trim through
/// `editor.seq_cut_edit` moves its later keys with the other keys.
#[test]
fn event_keys_select_copy_paste_and_ripple_through_the_handlers() {
    let (mut world, player) = ripple_world();
    if let Some(mut p) = world.get_mut::<SequencePlayer>(player) {
        let mut s = p.sequence().clone();
        s.tracks
            .push(event_track("cues", &[(1.0, "door"), (4.5, "light")]));
        p.replace_sequence(s);
    }
    let cues = sequence(&world, player).tracks.len() - 1;
    let state = seq_timeline_state(&mut world, &json!({})).expect("state");
    let row = state["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|r| r["name"] == "cues")
        .cloned();
    assert_eq!(
        row.as_ref().map(|r| (r["kind"].clone(), r["keys"].clone())),
        Some((json!("Event"), json!([1.0, 4.5]))),
        "the event track is a row with its keys: {state}"
    );

    let picked = seq_keys(
        &mut world,
        &json!({ "op": "select", "range": [0.9, 1.1], "tracks": [cues] }),
    )
    .expect("box select");
    assert_eq!(
        picked["selection"].as_array().map(Vec::len),
        Some(1),
        "the box picks `door`: {picked}"
    );
    let copied = seq_keys(&mut world, &json!({ "op": "copy" })).expect("copy");
    assert_eq!(copied["clipboard"], json!(1), "{copied}");
    let before = sequence(&world, player);
    let pasted = seq_keys(&mut world, &json!({ "op": "paste", "time": 4.5 })).expect("paste");
    assert_eq!(
        events_named(&world, player, "cues"),
        [
            (1.0, "door".to_string()),
            (4.5, "light".to_string()),
            (4.5, "door".to_string())
        ],
        "pasted after `light`, which was at 4.5 s first: {pasted}"
    );
    assert_eq!(depth(&world), 1, "one undo entry");
    assert!(undo_world(&mut world));
    assert_eq!(sequence(&world, player), before);

    // A rippling trim of the middle shot (2..4 s to 1.5 s) moves the event after 4 s with the keys.
    ripple_shots(&mut world, player);
    seq_cut_edit(
        &mut world,
        &json!({ "op": "trim", "shot": 1, "duration": 1.5, "ripple": true }),
    )
    .expect("trim");
    assert_eq!(
        events_named(&world, player, "cues"),
        [(1.0, "door".to_string()), (4.0, "light".to_string())],
        "the later event moves 0.5 s earlier, the earlier one stays"
    );
}
