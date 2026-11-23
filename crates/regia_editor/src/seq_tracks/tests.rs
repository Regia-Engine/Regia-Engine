// SPDX-License-Identifier: MPL-2.0
//! The acceptance of sequence authoring (ADR-9710): every new op through its agent handler, undo
//! of each restoring the sequence bytes, keys around a gizmo move playing back the sampler's
//! value, and a preview that leaves the level save byte-identical.

use regia_app::prelude::{App, MinimalPlugins};
use regia_framework::sequencer::{SequenceLibrary, SequencerPlugin};
use regia_scene::prelude::Transform;

use super::*;
use crate::agent_ext::EditorProject;
use crate::command::{SpawnEntities, undo_world};

fn bytes(world: &mut World) -> String {
    let panel = world
        .get_resource::<SeqCurvePanel>()
        .map(|p| p.player)
        .unwrap_or_default();
    let probe = SeqCurvePanel {
        player: panel,
        ..SeqCurvePanel::default()
    };
    let player = player_of(world, &probe).expect("a player");
    world
        .get::<SequencePlayer>(player)
        .expect("player")
        .sequence()
        .to_ron()
        .expect("ron")
}

fn code(e: RpcError) -> String {
    e.data
        .and_then(|d| d["code"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// A world with one player of an empty 5 s sequence and a cube bound as `cube`.
fn world_with_cube() -> (World, Entity, Entity) {
    let mut world = World::new();
    let sequence = tracks::new_sequence("intro", 5.0, 30.0).expect("new");
    let player = world.spawn(SequencePlayer::new(sequence)).id();
    let cube = world
        .spawn((Transform::default(), EntityGuid(Guid::new_random())))
        .id();
    seq_binding_edit(
        &mut world,
        &json!({ "op": "add", "binding": "cube", "entity": cube.to_bits() }),
    )
    .expect("bind");
    (world, player, cube)
}

fn add_params(kind: &str) -> Value {
    let mut p = json!({ "op": "add", "kind": kind, "name": format!("t_{kind}"), "binding": "cube", "at": 0 });
    match kind {
        "float" => p["property"] = json!("light.intensity"),
        "vec3" => p["property"] = json!("light.color"),
        "bool" => p["property"] = json!("visible"),
        "clip" => p["asset"] = json!("walk"),
        "audio" | "spatial_audio" => p["asset"] = json!("chime"),
        "sub" => p["asset"] = json!("cine/fx.seq.ron"),
        "video" => p["asset"] = json!("movies/a.webm"),
        "marker" => {
            p["name"] = json!("door");
            p["time"] = json!(1.5);
        }
        _ => {}
    }
    p
}

#[test]
fn every_track_kind_add_then_remove_is_byte_identical_and_undo_restores_each_step() {
    let (mut world, _, _) = world_with_cube();
    let start = bytes(&mut world);
    for kind in [
        "transform",
        "float",
        "vec3",
        "bool",
        "clip",
        "audio",
        "spatial_audio",
        "sub",
        "video",
        "marker",
    ] {
        let before = bytes(&mut world);
        seq_track_edit(&mut world, &add_params(kind)).unwrap_or_else(|e| panic!("{kind}: {e:?}"));
        let added = bytes(&mut world);
        assert_ne!(added, before, "{kind} added");
        let remove = if kind == "marker" {
            json!({ "op": "remove", "kind": "marker", "name": "door" })
        } else {
            json!({ "op": "remove", "track": format!("t_{kind}") })
        };
        seq_track_edit(&mut world, &remove).unwrap_or_else(|e| panic!("{kind}: {e:?}"));
        assert_eq!(
            bytes(&mut world),
            before,
            "{kind}: add then remove is byte-identical"
        );
        // Undo the removal, then the add: each restores the bytes of that step.
        assert!(undo_world(&mut world));
        assert_eq!(bytes(&mut world), added, "{kind}: undo of remove");
        assert!(undo_world(&mut world));
        assert_eq!(bytes(&mut world), before, "{kind}: undo of add");
    }
    assert_eq!(bytes(&mut world), start);
    // Fade is refused with a code and changes nothing.
    let err = seq_track_edit(&mut world, &add_params("fade")).expect_err("fade");
    assert_eq!(code(err), "seq_track_fade_in_shots");
    assert_eq!(bytes(&mut world), start);
}

#[test]
fn rename_and_move_round_trip_and_undo() {
    let (mut world, _, _) = world_with_cube();
    for kind in ["transform", "float"] {
        seq_track_edit(&mut world, &add_params(kind)).expect("add");
    }
    let before = bytes(&mut world);
    let moved =
        seq_track_edit(&mut world, &json!({ "op": "move", "track": "t_float", "to": 1 })).expect("move");
    assert_eq!(moved["tracks"][1]["name"], json!("t_float"));
    seq_track_edit(
        &mut world,
        &json!({ "op": "rename", "track": "t_float", "new_name": "glow" }),
    )
    .expect("rename");
    let err = seq_track_edit(
        &mut world,
        &json!({ "op": "rename", "track": "glow", "new_name": "t_transform" }),
    )
    .expect_err("taken");
    assert_eq!(code(err), "seq_track_name_taken");
    assert!(undo_world(&mut world));
    assert!(undo_world(&mut world));
    assert_eq!(bytes(&mut world), before, "undo of rename and move");
    // Markers rename and move too.
    seq_track_edit(
        &mut world,
        &json!({ "op": "add", "kind": "marker", "name": "a", "time": 1.0 }),
    )
    .expect("m");
    let m = bytes(&mut world);
    seq_track_edit(
        &mut world,
        &json!({ "op": "move", "kind": "marker", "name": "a", "to": 0 }),
    )
    .expect("mv");
    seq_track_edit(
        &mut world,
        &json!({ "op": "rename", "kind": "marker", "name": "a", "new_name": "b" }),
    )
    .expect("rn");
    assert!(undo_world(&mut world));
    assert!(undo_world(&mut world));
    assert_eq!(bytes(&mut world), m);
}

#[test]
fn removing_a_used_binding_without_cascade_is_refused_and_changes_nothing() {
    let (mut world, player, cube) = world_with_cube();
    seq_track_edit(&mut world, &add_params("transform")).expect("track");
    let before = bytes(&mut world);
    let table = world.get::<SequenceBindings>(player).cloned();
    let err = seq_binding_edit(&mut world, &json!({ "op": "remove", "binding": "cube" })).expect_err("used");
    assert_eq!(code(err), "seq_binding_used");
    assert_eq!(bytes(&mut world), before, "the sequence is unchanged");
    assert_eq!(
        world.get::<SequenceBindings>(player).cloned(),
        table,
        "the table is unchanged"
    );
    // A second binding of the same name is refused.
    let err = seq_binding_edit(
        &mut world,
        &json!({ "op": "add", "binding": "cube", "entity": cube.to_bits() }),
    )
    .expect_err("twice");
    assert_eq!(code(err), "seq_binding_in_use");
    // Cascade removes the track and the table entry; undo restores both.
    let out = seq_binding_edit(
        &mut world,
        &json!({ "op": "remove", "binding": "cube", "cascade": true }),
    )
    .expect("cascade");
    assert_eq!(out["removed"]["tracks"], json!(1));
    assert!(
        world
            .get::<SequenceBindings>(player)
            .is_none_or(|t| t.get("cube").is_none())
    );
    assert!(undo_world(&mut world));
    assert_eq!(bytes(&mut world), before);
    assert_eq!(world.get::<SequenceBindings>(player).cloned(), table);
    // A binding by guid survives: the table holds the cube's guid.
    let guid = world.get::<EntityGuid>(cube).expect("guid").0;
    assert_eq!(table.and_then(|t| t.get("cube")), Some(Binding::Guid(guid)));
    // A spawnable binding is a sequence change; its undo restores the bytes.
    seq_binding_edit(
        &mut world,
        &json!({ "op": "add", "binding": "crate", "prefab": "props/crate.prefab.ron" }),
    )
    .expect("spawnable");
    assert_ne!(bytes(&mut world), before);
    assert!(undo_world(&mut world));
    assert_eq!(bytes(&mut world), before);
}

#[test]
fn seq_new_writes_an_empty_v5_file_and_opens_it() {
    let root = regia_test_utils::unique_temp_dir("seq_tracks_new");
    let mut world = World::new();
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    let out = seq_new(
        &mut world,
        &json!({ "path": "cine/shot.seq.ron", "length": 4.0, "fps": 24.0 }),
    )
    .expect("new");
    assert_eq!(out["length"], json!(4.0));
    let text = std::fs::read_to_string(root.join("cine/shot.seq.ron")).expect("written");
    let sequence = Sequence::from_ron(&text).expect("valid");
    assert_eq!(
        sequence.version,
        regia_framework::sequencer::SEQUENCE_FORMAT_VERSION
    );
    assert_eq!((sequence.name.as_str(), sequence.fps), ("shot", 24.0));
    assert!(sequence.tracks.is_empty());
    assert_eq!(bytes(&mut world), text, "the panel holds the file it wrote");
    let err = seq_new(&mut world, &json!({ "path": "cine/shot.seq.ron" })).expect_err("exists");
    assert_eq!(code(err), "seq_new_exists");
    let err = seq_new(&mut world, &json!({ "path": "../out.seq.ron" })).expect_err("outside");
    assert_eq!(code(err), "seq_curve_bad_path");
    let err = seq_new(&mut world, &json!({ "path": "cine/b.seq.ron", "fps": 0.0 })).expect_err("fps");
    assert_eq!(code(err), "seq_new_bad_fps");
}

/// Moves `entity` the way a gizmo drag does: one `SetTransform` entry, sealed when the drag ends.
fn gizmo_move(world: &mut World, entity: Entity, to: [f32; 3]) {
    let new = Transform {
        translation: regia_core::math::Vec3::from_array(to),
        ..world.get::<Transform>(entity).copied().unwrap_or_default()
    };
    let mut command = SetTransform::capture(world, entity, new);
    command.apply(world);
    let mut undo = world.get_resource_or_insert_with(UndoStack::new);
    undo.push(Box::new(command));
    undo.seal();
}

#[test]
fn keys_around_a_gizmo_move_play_back_the_sampler_value() {
    let root = regia_test_utils::unique_temp_dir("seq_tracks_key");
    let mut world = World::new();
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    seq_new(&mut world, &json!({ "path": "cine/cube.seq.ron", "length": 3.0 })).expect("new");
    let cube = world
        .spawn((Transform::default(), EntityGuid(Guid::new_random())))
        .id();
    seq_binding_edit(
        &mut world,
        &json!({ "op": "add", "binding": "cube", "entity": cube.to_bits() }),
    )
    .expect("bind");
    let key = seq_key(&mut world, &json!({ "binding": "cube", "channel": "position" })).expect("key 0");
    assert_eq!(key["keys"][0]["created"], json!(true));
    assert_eq!(key["keys"][0]["time"], json!(0.0));
    gizmo_move(&mut world, cube, [4.0, 2.0, 0.0]);
    seq_preview(&mut world, &json!({ "on": true, "time": 2.0 })).expect("seek");
    seq_preview(&mut world, &json!({ "on": false })).expect("stop");
    // The preview put the moved value back.
    assert_eq!(
        world.get::<Transform>(cube).expect("t").translation.to_array(),
        [4.0, 2.0, 0.0]
    );
    let key = seq_key(&mut world, &json!({ "binding": "cube", "channel": "position" })).expect("key 2");
    assert_eq!(key["keys"][0]["time"], json!(2.0));
    assert_eq!(key["keys"][0]["created"], json!(false));
    let saved = crate::seq_curve_panel::methods()
        .into_iter()
        .find(|(m, _)| m.name == "editor.seq_curve_save")
        .map(|(_, h)| h)
        .expect("save method");
    saved(&mut world, &json!({})).expect("save");
    let text = std::fs::read_to_string(root.join("cine/cube.seq.ron")).expect("saved");
    let sequence = Sequence::from_ron(&text).expect("valid");
    let Some(ChannelValue::Vec3(expected)) = sequence.sample(1.0).values.first().map(|v| v.value) else {
        panic!("a position value at 1 s");
    };
    // Play the saved file headless with the runtime player.
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SequencerPlugin));
    let target = app.world_mut().spawn(Transform::default()).id();
    let player = app
        .world_mut()
        .spawn((
            SequencePlayer::new(sequence.clone()),
            SequenceBindings::default().with("cube", Binding::Entity(target)),
        ))
        .id();
    {
        // A paused player applies its frame; a stopped one applies nothing.
        let mut p = app.world_mut().get_mut::<SequencePlayer>(player).expect("player");
        p.play();
        p.pause();
        p.seek(1.0);
    }
    app.update();
    assert_eq!(
        app.world().get::<SequencePlayer>(player).expect("player").time(),
        1.0
    );
    let played = app
        .world()
        .get::<Transform>(target)
        .expect("t")
        .translation
        .to_array();
    assert_eq!(played, expected, "the cube at 1 s is the sampler's value");
    assert!(
        expected[0] > 0.0 && expected[0] < 4.0,
        "between the keys: {expected:?}"
    );
}

#[test]
fn auto_key_joins_the_gizmo_edit_and_its_key_in_one_undo_entry() {
    let (mut world, _, cube) = world_with_cube();
    let before = bytes(&mut world);
    let edits = world.resource::<UndoStack>().undo_depth();
    seq_key(&mut world, &json!({ "auto": true })).expect("auto on");
    gizmo_move(&mut world, cube, [1.0, 0.0, 0.0]);
    let keys = auto_key_tick(&mut world);
    assert_eq!(keys.len(), 3, "position, rotation and scale: {keys:?}");
    assert_eq!(auto_key_tick(&mut world), Vec::new(), "keyed once");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), edits + 1, "one entry");
    assert_ne!(bytes(&mut world), before);
    assert!(undo_world(&mut world));
    assert_eq!(bytes(&mut world), before, "the key is undone");
    assert_eq!(
        world.get::<Transform>(cube).expect("t").translation.to_array(),
        [0.0, 0.0, 0.0],
        "and the move with it"
    );
    // An undo is not a new edit: nothing is keyed.
    assert_eq!(auto_key_tick(&mut world), Vec::new());
    // With auto off, a move keys nothing.
    seq_key(&mut world, &json!({ "auto": false })).expect("auto off");
    gizmo_move(&mut world, cube, [2.0, 0.0, 0.0]);
    assert_eq!(auto_key_tick(&mut world), Vec::new());
    // An entity the sequence does not bind keys nothing.
    seq_key(&mut world, &json!({ "auto": true })).expect("auto on");
    let other = world.spawn(Transform::default()).id();
    gizmo_move(&mut world, other, [3.0, 0.0, 0.0]);
    assert_eq!(auto_key_tick(&mut world), Vec::new());
}

#[test]
fn key_refusals_change_nothing() {
    let (mut world, _, _) = world_with_cube();
    let before = bytes(&mut world);
    let err = seq_key(&mut world, &json!({ "binding": "ghost" })).expect_err("unbound");
    assert_eq!(code(err), "seq_key_unbound");
    let err = seq_key(
        &mut world,
        &json!({ "binding": "cube", "channel": "light.intensity" }),
    )
    .expect_err("no light");
    assert_eq!(code(err), "seq_key_unreadable");
    let err = seq_key(&mut world, &json!({})).expect_err("no selection");
    assert_eq!(code(err), "seq_key_no_selection");
    assert_eq!(bytes(&mut world), before);
}

#[test]
fn scrub_then_stop_leaves_the_level_save_byte_identical() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>();
    app.finish_plugins();
    let world = app.world_mut();
    let open = crate::level_io::new_level(world);
    let guid = Guid::new_random();
    let mut spawn = SpawnEntities::new_entity(world, guid, "Cube", Transform::default()).expect("spawn");
    spawn.apply(world);
    let cube = {
        let mut query = world.query::<(Entity, &EntityGuid)>();
        query
            .iter(world)
            .find(|(_, g)| g.0 == guid)
            .map(|(e, _)| e)
            .expect("cube")
    };
    let dir = regia_test_utils::unique_temp_dir("seq_tracks_preview");
    let level = dir.join("room.level.ron");
    crate::level_io::save_level_file(world, &open, &level).expect("save");
    let saved = std::fs::read(&level).expect("read");
    assert!(
        String::from_utf8_lossy(&saved).contains(&guid.to_string()),
        "the cube is in the level file"
    );
    // A sequence that moves, rotates and hides the cube.
    let sequence = tracks::new_sequence("p", 2.0, 30.0).expect("new");
    world.spawn(SequencePlayer::new(sequence));
    world.insert_resource(SequenceLibrary::default());
    seq_binding_edit(
        world,
        &json!({ "op": "add", "binding": "cube", "entity": cube.to_bits() }),
    )
    .expect("bind");
    for (kind, name, property, value) in [
        ("transform", "move", "transform.position", json!([3.0, 1.0, 0.0])),
        (
            "transform",
            "turn",
            "transform.rotation",
            json!([10.0, 45.0, 0.0]),
        ),
        ("bool", "hide", "visible", json!(false)),
    ] {
        seq_track_edit(
            world,
            &json!({ "op": "add", "kind": kind, "name": name, "binding": "cube", "property": property, "value": value }),
        )
        .expect("track");
    }
    let original = *world.get::<Transform>(cube).expect("t");
    let on = seq_preview(world, &json!({ "on": true, "time": 1.0 })).expect("preview");
    assert_eq!(on["touched"], json!([cube.to_bits()]));
    let moved = *world.get::<Transform>(cube).expect("t");
    assert_eq!(
        moved.translation.to_array(),
        [3.0, 1.0, 0.0],
        "the preview moved the cube"
    );
    assert_ne!(moved.rotation, original.rotation);
    // The previewed values are in the world: a snapshot without the lift would save them.
    let raw = regia_scene::level::snapshot_level(world, open.id)
        .expect("snapshot")
        .to_ron_string()
        .expect("ron");
    assert_ne!(
        raw.as_bytes(),
        saved.as_slice(),
        "the preview changed level state"
    );
    // A save during the preview sees the level as it was.
    crate::level_io::save_level_file(world, &open, &level).expect("save during preview");
    assert_eq!(
        std::fs::read(&level).expect("read"),
        saved,
        "no previewed value in the file"
    );
    assert_eq!(
        world.get::<Transform>(cube).expect("t").translation.to_array(),
        [3.0, 1.0, 0.0],
        "the preview is back after the save"
    );
    // Scrub, then stop.
    seq_preview(world, &json!({ "on": true, "time": 0.5 })).expect("scrub");
    let off = seq_preview(world, &json!({ "on": false })).expect("stop");
    assert_eq!(off["restored"], json!(1));
    assert_eq!(
        *world.get::<Transform>(cube).expect("t"),
        original,
        "bit-exact transform"
    );
    crate::level_io::save_level_file(world, &open, &level).expect("save after");
    assert_eq!(
        std::fs::read(&level).expect("read"),
        saved,
        "byte-identical level save"
    );
    // An unrelated edit made during a preview stays when the preview stops.
    seq_preview(world, &json!({ "on": true, "time": 1.0 })).expect("preview again");
    world
        .entity_mut(cube)
        .insert(regia_scene::components::Name::new("Renamed"));
    seq_preview(world, &json!({ "on": false })).expect("stop");
    assert_eq!(
        world
            .get::<regia_scene::components::Name>(cube)
            .map(|n| n.as_str().to_string()),
        Some("Renamed".to_string()),
        "the rename survives the restore"
    );
    assert_eq!(
        *world.get::<Transform>(cube).expect("t"),
        original,
        "the transform is back"
    );
}

#[test]
fn every_new_wire_name_reaches_its_handler() {
    let (mut world, _, _) = world_with_cube();
    for (schema, handler) in methods() {
        assert!(schema.name.starts_with("editor.seq_"), "{}", schema.name);
        // An empty call is answered (a refusal is fine), never a panic.
        let _ = handler(&mut world, &json!({}));
    }
    let names: Vec<&str> = methods().iter().map(|(m, _)| m.name).collect();
    assert_eq!(
        names,
        [
            "editor.seq_track_edit",
            "editor.seq_binding_edit",
            "editor.seq_new",
            "editor.seq_key",
            "editor.seq_preview"
        ]
    );
}

#[test]
fn closing_the_panel_ends_the_preview_and_puts_values_back() {
    let (mut world, _, cube) = world_with_cube();
    seq_track_edit(
        &mut world,
        &json!({ "op": "add", "kind": "transform", "name": "move", "binding": "cube", "value": [5.0, 0.0, 0.0] }),
    )
    .expect("track");
    // An agent-only preview (the panel never drew) is not ended by the shell.
    seq_preview(&mut world, &json!({ "on": true })).expect("preview");
    crate::seq_tracks::end_preview_when_hidden(&mut world, 10);
    assert_eq!(
        world.get::<Transform>(cube).expect("t").translation.to_array(),
        [5.0, 0.0, 0.0],
        "still previewing"
    );
    // The panel draws (the authoring rows note the frame), then stops drawing.
    let ctx = egui::Context::default();
    let frame = |world: &mut World| {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| crate::seq_curve_panel::seq_curve_panel(ui, world));
        });
        output.textures_delta.clear();
        ctx.cumulative_frame_nr()
    };
    frame(&mut world);
    let drawn = frame(&mut world);
    let noted = world
        .resource::<SeqAuthoring>()
        .preview
        .as_ref()
        .and_then(|p| p.drawn_frame);
    assert!(noted.is_some(), "the rows noted the frame they drew in");
    crate::seq_tracks::end_preview_when_hidden(&mut world, noted.unwrap_or(drawn));
    assert!(
        world.resource::<SeqAuthoring>().preview.is_some(),
        "drawn this frame: kept"
    );
    crate::seq_tracks::end_preview_when_hidden(&mut world, noted.unwrap_or(drawn) + 1);
    assert!(
        world.resource::<SeqAuthoring>().preview.is_none(),
        "not drawn: ended"
    );
    assert_eq!(
        world.get::<Transform>(cube).expect("t").translation.to_array(),
        [0.0, 0.0, 0.0],
        "the value is back"
    );
}

/// w7 item 5: the edit-world preview poses the rig a clip section plays (`SequenceClips`), blends
/// over the rig's own pose every time (no feedback from the previous preview frame), and Stop puts
/// every joint back bit-exact.
#[test]
fn the_preview_poses_clip_sections_and_stop_puts_the_joints_back() {
    use std::sync::Arc;

    use regia_animation::clip::{JointTrack, TrackCurve};
    use regia_animation::components::spawn_skeleton;
    use regia_animation::pose::translated;
    use regia_animation::skeleton::Skeleton;
    use regia_animation::{AnimationClip, SkinnedMesh};
    use regia_core::math::{Quat, Vec3};
    use regia_framework::sequencer::{Binding, Channel, ClipSection, SequenceBindings, SequenceClips, Track};

    let mut world = World::new();
    let names = (0..3).map(|i| format!("j{i}")).collect();
    let parents = (0..3)
        .map(|i: usize| i.checked_sub(1).map(|p| p as u32))
        .collect();
    let bind = (0..3).map(|_| translated(Vec3::new(0.0, 0.1, 0.0))).collect();
    let skeleton = Arc::new(Skeleton::from_local_bind(names, parents, bind).expect("skeleton"));
    let root = world
        .spawn((Transform::default(), EntityGuid(Guid::new_random())))
        .id();
    let binding: SkinnedMesh = spawn_skeleton(&mut world, skeleton);
    let joints = binding.joints.clone();
    world.entity_mut(root).insert(binding);
    let rest: Vec<Transform> = joints
        .iter()
        .map(|j| world.get::<Transform>(*j).copied().expect("joint"))
        .collect();

    let mut wave = AnimationClip::empty("wave", 1.0);
    wave.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Static(Vec3::new(2.0, 0.1, 0.0)),
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    world
        .get_resource_or_insert_with(SequenceClips::default)
        .insert("wave", wave);
    let mut sequence = tracks::new_sequence("intro", 3.0, 30.0).expect("new");
    sequence.tracks.push(Track {
        name: "body".into(),
        target: "hero".into(),
        property: String::new(),
        keys: Channel::Clip(vec![ClipSection {
            clip: "wave".into(),
            start: 0.0,
            end: 2.0,
            offset: 0.0,
            speed: 1.0,
            looping: false,
            blend_in: 0.0,
            blend_out: 1.0,
            weight: 1.0,
        }]),
        enabled: true,
        locked: false,
    });
    world.spawn((
        SequencePlayer::new(sequence),
        SequenceBindings::default().with("hero", Binding::Entity(root)),
    ));
    let x = |world: &World| world.get::<Transform>(joints[1]).expect("j1").translation.x;

    seq_preview(&mut world, &json!({ "on": true, "time": 0.5 })).expect("preview");
    assert_eq!(x(&world), 2.0, "full weight: the clip's pose");
    // t = 1.5: half way through the blend out, over the rig's own pose (x = 0).
    seq_preview(&mut world, &json!({ "on": true, "time": 1.5 })).expect("scrub");
    assert!((x(&world) - 1.0).abs() < 1.0e-5, "half and half: {}", x(&world));
    // Another time and back: the same pose (a blend over the last frame would drift).
    seq_preview(&mut world, &json!({ "on": true, "time": 1.25 })).expect("scrub");
    seq_preview(&mut world, &json!({ "on": true, "time": 1.5 })).expect("scrub");
    assert!((x(&world) - 1.0).abs() < 1.0e-5, "no feedback: {}", x(&world));
    // Past the section: the rig's own pose again.
    seq_preview(&mut world, &json!({ "on": true, "time": 2.5 })).expect("scrub");
    assert_eq!(x(&world), 0.0, "outside the section");
    seq_preview(&mut world, &json!({ "on": true, "time": 0.5 })).expect("scrub");
    assert_eq!(x(&world), 2.0);

    seq_preview(&mut world, &json!({ "on": false })).expect("stop");
    for (joint, rest) in joints.iter().zip(&rest) {
        assert_eq!(world.get::<Transform>(*joint), Some(rest), "joint back bit-exact");
    }
}

/// Lane w7-editor-smalls-z (3): a GLB export made while a sequence is previewed (the agent's
/// `asset.export_glb` of the open level, and File → Export selection) writes the transforms as
/// they were before the preview, and the preview is still on, unchanged, afterwards.
#[test]
fn a_glb_export_during_a_preview_writes_the_pre_preview_transforms() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>();
    app.finish_plugins();
    let world = app.world_mut();
    let open = crate::level_io::new_level(world);
    world.insert_resource(open);
    let guid = Guid::new_random();
    let mut spawn = SpawnEntities::new_entity(world, guid, "Cube", Transform::default()).expect("spawn");
    spawn.apply(world);
    let cube = {
        let mut query = world.query::<(Entity, &EntityGuid)>();
        query
            .iter(world)
            .find(|(_, g)| g.0 == guid)
            .map(|(e, _)| e)
            .expect("cube")
    };
    let root = regia_test_utils::unique_temp_dir("seq_tracks_export_preview");
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.clone()),
    });
    let mut selection = crate::selection::Selection::default();
    selection.select(cube, crate::selection::SelectMode::Replace);
    world.insert_resource(selection);
    let (_, export_glb) = crate::export_glb::agent_method();
    let export = |world: &mut World, name: &str| {
        let out = export_glb(world, &json!({ "out": format!("export/{name}.glb") })).expect("export");
        assert_eq!(out["ok"], true, "{out}");
        let level = std::fs::read(root.join(format!("export/{name}.glb"))).expect("level glb");
        let picked = root.join(format!("export/{name}_selection.glb"));
        crate::export_glb::export_selection(world, &root, &picked).expect("export selection");
        (level, std::fs::read(&picked).expect("selection glb"))
    };
    let before = export(world, "before");

    let sequence = tracks::new_sequence("p", 2.0, 30.0).expect("new");
    world.spawn(SequencePlayer::new(sequence));
    world.insert_resource(SequenceLibrary::default());
    seq_binding_edit(
        world,
        &json!({ "op": "add", "binding": "cube", "entity": cube.to_bits() }),
    )
    .expect("bind");
    seq_track_edit(
        world,
        &json!({ "op": "add", "kind": "transform", "name": "move", "binding": "cube", "property": "transform.position", "value": [3.0, 1.0, 0.0] }),
    )
    .expect("track");
    seq_preview(world, &json!({ "on": true, "time": 1.0 })).expect("preview");
    assert_eq!(
        world.get::<Transform>(cube).expect("t").translation.to_array(),
        [3.0, 1.0, 0.0],
        "the preview moved the cube"
    );
    let during = export(world, "during");
    assert!(
        during.0 == before.0,
        "asset.export_glb during the preview writes the pre-preview transform"
    );
    assert!(
        during.1 == before.1,
        "File → Export selection during the preview writes the pre-preview transform"
    );
    assert_eq!(
        world.get::<Transform>(cube).expect("t").translation.to_array(),
        [3.0, 1.0, 0.0],
        "the preview is back after the export"
    );
    let preview = world
        .resource::<SeqAuthoring>()
        .preview
        .as_ref()
        .map(SeqPreview::touched);
    assert_eq!(preview, Some(vec![cube]), "the preview is still on");
    seq_preview(world, &json!({ "on": false })).expect("stop");
    assert_eq!(
        *world.get::<Transform>(cube).expect("t"),
        Transform::default(),
        "the stop restores the pre-preview transform"
    );
}

/// Lane w7-editor-smalls-z (4): `editor.seq_new` refuses an empty or blank `name` with
/// `seq_new_name_empty` and writes no file.
#[test]
fn seq_new_refuses_an_empty_or_blank_name() {
    let mut world = World::new();
    let dir = regia_test_utils::unique_temp_dir("seq_new_blank_name");
    world.insert_resource(EditorProject {
        root: Some(dir.clone()),
    });
    for name in ["", "  "] {
        let err = seq_new(&mut world, &json!({ "path": "cine/blank.seq.ron", "name": name }))
            .expect_err("blank name");
        assert_eq!(code(err), "seq_new_name_empty", "{name:?}");
        assert!(!dir.join("cine/blank.seq.ron").exists(), "no file written");
    }
    seq_new(&mut world, &json!({ "path": "cine/blank.seq.ron", "name": "ok" })).expect("a named sequence");
}

/// Runs one editor agent method through its installed handler (the dispatcher's table).
fn call(world: &mut World, method: &str, params: Value) -> Value {
    let (_, handler) = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is installed"));
    handler(world, &params).unwrap_or_else(|e| panic!("{method} {params}: {}", e.message))
}

/// The preview's voice counters, `(started, stopped)`.
fn voice_counts(out: &Value) -> (Option<u64>, Option<u64>) {
    (
        out["voices"]["started"].as_u64(),
        out["voices"]["stopped"].as_u64(),
    )
}

/// w8 item 3: the edit-world preview plays flat and spatial sound sections on the editor's audio
/// output (the edit world's `Audio`, here the headless backend): a scrub into a section starts its
/// voice at the section offset, playing on keeps it, a scrub inside restarts it at the new offset,
/// a scrub out and Stop stop it. Counted, no wall clock.
#[test]
fn the_preview_plays_flat_and_spatial_sound_sections_and_counts_voices() {
    use regia_audio::{Audio, AudioSettings, SoundAsset, SoundAssets, SoundClip};
    use regia_framework::sequencer::{
        AudioSection, AudioSpatial, Binding, Channel, SequenceBindings, SoundSource, Track,
    };

    let mut world = World::new();
    world.insert_resource(Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    }));
    let clip = SoundClip::new(vec![0.1; 48_000 * 4], 48_000).expect("clip");
    let mut sounds = SoundAssets::default();
    sounds.add("chime", SoundAsset::from_clip(&clip));
    world.insert_resource(sounds);
    let speaker = world
        .spawn(Transform::from_translation(regia_core::math::Vec3::new(
            3.0, 0.0, 0.0,
        )))
        .id();
    let mut sequence = tracks::new_sequence("intro", 6.0, 30.0).expect("new");
    let audio_track = |name: &str, section: AudioSection| Track {
        name: name.into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Audio(vec![section]),
        enabled: true,
        locked: false,
    };
    sequence
        .tracks
        .push(audio_track("music", AudioSection::new("chime", 1.0, 3.0)));
    let mut door = AudioSection::new("chime", 1.0, 3.0);
    door.spatial = Some(AudioSpatial::new(SoundSource::Binding("speaker".into())));
    sequence.tracks.push(audio_track("door", door));
    world.spawn((
        SequencePlayer::new(sequence),
        SequenceBindings::default().with("speaker", Binding::Entity(speaker)),
    ));

    // Before the sections: nothing plays.
    let out = seq_preview(&mut world, &json!({ "on": true, "time": 0.5 })).expect("preview");
    assert_eq!(voice_counts(&out), (Some(0), Some(0)), "{out}");
    assert_eq!(out["sounds"], json!([]), "{out}");
    // Scrub in: both sections start, at the section offset.
    let out = seq_preview(&mut world, &json!({ "on": true, "time": 1.5 })).expect("scrub in");
    assert_eq!(voice_counts(&out), (Some(2), Some(0)), "{out}");
    let playing = out["sounds"].as_array().cloned().unwrap_or_default();
    assert_eq!(playing.len(), 2, "{out}");
    let music = playing.iter().find(|s| s["track"] == "music").expect("music");
    assert_eq!(music["sound"], "chime");
    assert_eq!(music["spatial"], false);
    assert!(
        (music["position"].as_f64().unwrap_or(-1.0) - 0.5).abs() < 1.0e-6,
        "{music}"
    );
    let door = playing.iter().find(|s| s["track"] == "door").expect("door");
    assert_eq!(door["spatial"], true);
    assert_eq!(
        door["emitter"],
        json!([3.0, 0.0, 0.0]),
        "at the bound emitter: {door}"
    );

    // Play: the transport's ticks inside the section keep the voices.
    call(&mut world, "editor.seq_transport", json!({ "op": "play" }));
    for _ in 0..5 {
        call(
            &mut world,
            "editor.seq_transport",
            json!({ "op": "tick", "dt": 0.1 }),
        );
    }
    call(&mut world, "editor.seq_transport", json!({ "op": "pause" }));
    let out = seq_preview(&mut world, &json!({ "on": true })).expect("state");
    // w10 (sprint-w9-rev-v06-lanes § 1 finding 1): the ticks kept both voices (none restarted:
    // 2 started), and Pause stops them (the lane asserted `(2, 0)`, "playing on", which pinned
    // the bug; corrected, not weakened).
    assert_eq!(voice_counts(&out), (Some(2), Some(2)), "paused: silent: {out}");
    assert_eq!(out["sounds"], json!([]), "paused: {out}");

    // A scrub inside the section restarts each voice at its new offset.
    let out = seq_preview(&mut world, &json!({ "on": true, "time": 2.5 })).expect("scrub");
    assert_eq!(voice_counts(&out), (Some(4), Some(2)), "{out}");
    let music = out["sounds"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["track"] == "music").cloned())
        .expect("music");
    assert!(
        (music["position"].as_f64().unwrap_or(-1.0) - 1.5).abs() < 1.0e-6,
        "{music}"
    );
    // Scrub out: both stop.
    let out = seq_preview(&mut world, &json!({ "on": true, "time": 4.0 })).expect("scrub out");
    assert_eq!(voice_counts(&out), (Some(4), Some(4)), "{out}");
    assert_eq!(out["sounds"], json!([]), "{out}");
    // In again, then Stop: stopped.
    let out = seq_preview(&mut world, &json!({ "on": true, "time": 2.0 })).expect("in again");
    assert_eq!(voice_counts(&out), (Some(6), Some(4)), "{out}");
    let out = seq_preview(&mut world, &json!({ "on": false })).expect("stop");
    assert_eq!(
        voice_counts(&out),
        (Some(6), Some(6)),
        "Stop stops every voice: {out}"
    );
}

/// w8 item 4: the edit-world preview shows the picture of a video section's source time on its
/// bound video player (frame-exact decode with `regia_media`'s seek) and Stop gives the player
/// back as it was. Compared by frame index and presentation time, not pixels.
#[test]
fn the_preview_shows_the_video_frame_of_the_section_time_on_its_bound_player() {
    use regia_framework::sequencer::{Binding, Channel, SequenceBindings, Track, VideoSection, VideoTarget};
    use regia_media::mux::{PatternClip, pattern_index};
    use regia_media::{VideoPlayer, VideoPlayerState};

    let root = regia_test_utils::unique_temp_dir("seq-preview-video");
    std::fs::create_dir_all(root.join("movies")).expect("dir");
    std::fs::write(root.join("Regia.toml"), "[project]\nname = \"g\"\n").expect("manifest");
    // 40 frames at 25 fps: frame n shows at n × 40 ms.
    std::fs::write(
        root.join("movies/a.webm"),
        PatternClip::new(16, 16, 40, 25).to_webm(),
    )
    .expect("clip");
    let mut world = World::new();
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    let own = VideoPlayer {
        source: "movies/own.webm".into(),
        autoplay: false,
        ..VideoPlayer::default()
    };
    let tv = world.spawn((Transform::default(), own.clone())).id();
    let mut sequence = tracks::new_sequence("intro", 6.0, 30.0).expect("new");
    let mut section = VideoSection::new("movies/a.webm", 1.0, 2.6);
    section.offset = 0.2;
    section.target = VideoTarget::Player("tv".into());
    sequence.tracks.push(Track {
        name: "screen".into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Video(vec![section]),
        enabled: true,
        locked: false,
    });
    world.spawn((
        SequencePlayer::new(sequence),
        SequenceBindings::default().with("tv", Binding::Entity(tv)),
    ));
    let shown = |world: &World| -> (Option<u64>, Option<i64>, Option<u32>) {
        let state = world.get::<VideoPlayerState>(tv).expect("state");
        let core = state.core();
        (
            core.frame_index(),
            core.presented().map(|p| p.pts_ns),
            core.presented().map(|p| pattern_index(&p.planes[0])),
        )
    };

    // t = 1.5: section time 0.5 + offset 0.2 = 0.7 s into the source: frame 17 (680 ms).
    let out = seq_preview(&mut world, &json!({ "on": true, "time": 1.5 })).expect("preview");
    // w11 item 1c (ADR-7847): the section plays in the bound entity's session; its authored
    // `VideoPlayer` is never written (the w8 lane asserted the section's source on it, which
    // pinned the takeover that leaked into undo and prefab snapshots; corrected, not weakened).
    assert_eq!(
        world.get::<VideoPlayer>(tv),
        Some(&own),
        "the authored player stays as it is: {out}"
    );
    assert_eq!(shown(&world), (Some(17), Some(680_000_000), Some(17)), "{out}");
    let videos = out["videos"].as_array().cloned().unwrap_or_default();
    assert_eq!(videos.len(), 1, "{out}");
    assert_eq!(videos[0]["frame"], 17, "{out}");
    // Scrub back: t = 1.1 → 0.3 s into the source: frame 7 (280 ms).
    seq_preview(&mut world, &json!({ "on": true, "time": 1.1 })).expect("scrub");
    assert_eq!(shown(&world), (Some(7), Some(280_000_000), Some(7)));
    // Out of the section: the player is given back.
    let out = seq_preview(&mut world, &json!({ "on": true, "time": 3.0 })).expect("out");
    assert_eq!(world.get::<VideoPlayer>(tv), Some(&own), "{out}");
    assert_eq!(out["videos"], json!([]), "{out}");
    // In again, then Stop: the player is back as it was, with nothing decoded.
    seq_preview(&mut world, &json!({ "on": true, "time": 2.0 })).expect("in");
    assert_eq!(shown(&world).0, Some(30), "1.2 s: frame 30");
    seq_preview(&mut world, &json!({ "on": false })).expect("stop");
    assert_eq!(
        world.get::<VideoPlayer>(tv),
        Some(&own),
        "Stop restores the player"
    );
    assert_eq!(shown(&world), (None, None, None), "nothing decoded after Stop");
    let _ = std::fs::remove_dir_all(&root);
}

/// w8 item 5: a rig with an `Animator` is posed by its clip section while previewing (the
/// animator is suspended: it neither drives nor receives the pose), and Stop leaves its joints and
/// its animator bit-for-bit as they were.
#[test]
fn the_preview_poses_an_animator_rig_and_stop_restores_it_bit_for_bit() {
    use std::sync::Arc;

    use regia_animation::clip::{JointTrack, TrackCurve};
    use regia_animation::components::spawn_skeleton;
    use regia_animation::pose::translated;
    use regia_animation::skeleton::Skeleton;
    use regia_animation::{AnimationClip, Animator, SkinnedMesh};
    use regia_core::math::{Quat, Vec3};
    use regia_framework::sequencer::{Binding, Channel, ClipSection, SequenceBindings, SequenceClips, Track};

    let mut world = World::new();
    let names = (0..3).map(|i| format!("j{i}")).collect();
    let parents = (0..3)
        .map(|i: usize| i.checked_sub(1).map(|p| p as u32))
        .collect();
    let bind = (0..3).map(|_| translated(Vec3::new(0.0, 0.1, 0.0))).collect();
    let skeleton = Arc::new(Skeleton::from_local_bind(names, parents, bind).expect("skeleton"));
    let root = world
        .spawn((Transform::default(), EntityGuid(Guid::new_random())))
        .id();
    let binding: SkinnedMesh = spawn_skeleton(&mut world, skeleton);
    let joints = binding.joints.clone();
    world.entity_mut(root).insert(binding);
    // The rig's own animator, with an evaluated pose of its own (its state must survive).
    let mut idle = AnimationClip::empty("idle", 2.0);
    idle.tracks.push(JointTrack {
        joint: 2,
        translations: TrackCurve::Static(Vec3::new(0.0, 0.5, 0.0)),
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    let mut animator = Animator::single(idle, true);
    animator.speed = 0.75;
    animator.set("Speed", 3.25_f32);
    world.entity_mut(root).insert(animator);
    // A joint pose that is not the bind pose.
    world.get_mut::<Transform>(joints[1]).expect("j1").translation = Vec3::new(0.25, 0.1, -0.125);
    let rest: Vec<Transform> = joints
        .iter()
        .map(|j| world.get::<Transform>(*j).copied().expect("joint"))
        .collect();
    let animator_before = format!("{:?}", world.get::<Animator>(root).expect("animator"));

    let mut wave = AnimationClip::empty("wave", 1.0);
    wave.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Static(Vec3::new(2.0, 0.1, 0.0)),
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    world
        .get_resource_or_insert_with(SequenceClips::default)
        .insert("wave", wave);
    let mut sequence = tracks::new_sequence("intro", 3.0, 30.0).expect("new");
    sequence.tracks.push(Track {
        name: "body".into(),
        target: "hero".into(),
        property: String::new(),
        keys: Channel::Clip(vec![ClipSection {
            clip: "wave".into(),
            start: 0.0,
            end: 2.0,
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
    world.spawn((
        SequencePlayer::new(sequence),
        SequenceBindings::default().with("hero", Binding::Entity(root)),
    ));

    let out = seq_preview(&mut world, &json!({ "on": true, "time": 0.5 })).expect("preview");
    assert_eq!(
        world.get::<Transform>(joints[1]).map(|t| t.translation),
        Some(Vec3::new(2.0, 0.1, 0.0)),
        "the clip section poses the animator rig: {out}"
    );
    assert_eq!(out["animators"], json!([root.to_bits()]), "{out}");
    assert_eq!(
        format!("{:?}", world.get::<Animator>(root).expect("animator")),
        animator_before,
        "the animator is not written while suspended"
    );
    // Out of the section: the rig's own joints again.
    seq_preview(&mut world, &json!({ "on": true, "time": 2.5 })).expect("out");
    assert_eq!(world.get::<Transform>(joints[1]), Some(&rest[1]));
    seq_preview(&mut world, &json!({ "on": true, "time": 1.0 })).expect("in");

    seq_preview(&mut world, &json!({ "on": false })).expect("stop");
    for (joint, rest) in joints.iter().zip(&rest) {
        let now = world.get::<Transform>(*joint).copied().expect("joint");
        let bits = |t: &Transform| {
            let mut v = Vec::new();
            v.extend(t.translation.to_array().map(f32::to_bits));
            v.extend(t.rotation.to_array().map(f32::to_bits));
            v.extend(t.scale.to_array().map(f32::to_bits));
            v
        };
        assert_eq!(bits(&now), bits(rest), "joint back bit for bit");
    }
    assert_eq!(
        format!("{:?}", world.get::<Animator>(root).expect("animator kept")),
        animator_before,
        "the animator state is back bit for bit"
    );
}

/// w10 (sprint-w9-rev-v06-lanes § 1 finding 1): Pause, transport Stop and a non-looping
/// transport reaching the end of its range silence the preview's voices; a re-apply at the same
/// time (the state call, an edit, a level save's lift) keeps them silent; Play resumes them at the
/// current offset, and the ticks after keep them. Counted on the headless output, no wall clock.
#[test]
fn pause_stop_and_the_end_of_play_silence_the_preview_voices_and_play_resumes_them() {
    use regia_audio::{Audio, AudioSettings, SoundAsset, SoundAssets, SoundClip};
    use regia_framework::sequencer::{AudioSection, Channel, Track};

    let mut world = World::new();
    world.insert_resource(Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    }));
    let clip = SoundClip::new(vec![0.1; 48_000 * 4], 48_000).expect("clip");
    let mut sounds = SoundAssets::default();
    sounds.add("chime", SoundAsset::from_clip(&clip));
    world.insert_resource(sounds);
    let mut sequence = tracks::new_sequence("intro", 6.0, 30.0).expect("new");
    sequence.tracks.push(Track {
        name: "music".into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Audio(vec![AudioSection::new("chime", 1.0, 3.0)]),
        enabled: true,
        locked: false,
    });
    world.spawn(SequencePlayer::new(sequence));
    let transport = |world: &mut World, params: Value| call(world, "editor.seq_transport", params);
    let tick = |world: &mut World| transport(world, json!({ "op": "tick", "dt": 0.1 }));
    let state = |world: &mut World| seq_preview(world, &json!({ "on": true })).expect("state");
    let music_at = |out: &Value| -> Option<f64> {
        out["sounds"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["track"] == "music"))
            .and_then(|m| m["position"].as_f64())
    };

    // In the section: the voice starts; Play's ticks keep it.
    let out = seq_preview(&mut world, &json!({ "on": true, "time": 1.2 })).expect("preview");
    assert_eq!(voice_counts(&out), (Some(1), Some(0)), "{out}");
    transport(&mut world, json!({ "op": "play" }));
    for _ in 0..3 {
        tick(&mut world);
    }
    assert_eq!(voice_counts(&state(&mut world)), (Some(1), Some(0)), "playing on");

    // Pause: silent, and silent through re-applies at the same time.
    transport(&mut world, json!({ "op": "pause" }));
    for _ in 0..3 {
        let out = state(&mut world);
        assert_eq!(voice_counts(&out), (Some(1), Some(1)), "paused: {out}");
        assert_eq!(out["sounds"], json!([]), "paused: {out}");
        // A paused panel's frame tick moves nothing and starts nothing.
        tick(&mut world);
    }
    with_preview_lifted(&mut world, |_| {});
    assert_eq!(
        voice_counts(&state(&mut world)),
        (Some(1), Some(1)),
        "a lift keeps it silent"
    );

    // Resume: one voice again, at the offset of the playhead (1.5 s: 0.5 s into the section,
    // then one tick on).
    transport(&mut world, json!({ "op": "play" }));
    tick(&mut world);
    let out = state(&mut world);
    assert_eq!(voice_counts(&out), (Some(2), Some(1)), "resumed: {out}");
    let at = music_at(&out).expect("music plays");
    assert!(
        (at - 0.6).abs() < 1.0e-4,
        "resumed at the playhead's offset: {out}"
    );
    tick(&mut world);
    assert_eq!(voice_counts(&state(&mut world)), (Some(2), Some(1)), "playing on");

    // Toggle off (Space/K) is a pause too.
    transport(&mut world, json!({ "op": "toggle" }));
    assert_eq!(
        voice_counts(&state(&mut world)),
        (Some(2), Some(2)),
        "toggled off"
    );

    // A non-looping play range that ends inside the section: the end stops the voice.
    transport(&mut world, json!({ "op": "range", "in": 1.5, "out": 2.5 }));
    transport(&mut world, json!({ "op": "loop", "on": false }));
    transport(&mut world, json!({ "op": "seek", "time": 1.5 }));
    transport(&mut world, json!({ "op": "play" }));
    let started = voice_counts(&state(&mut world)).0.unwrap_or(0);
    for _ in 0..20 {
        tick(&mut world);
    }
    let out = state(&mut world);
    assert_eq!(out["sounds"], json!([]), "the play reached its end: {out}");
    let (s, stopped) = voice_counts(&out);
    assert_eq!(s, Some(started), "no voice started at the end: {out}");
    assert_eq!(stopped, s, "every started voice is stopped: {out}");

    // Transport Stop (back to the range start, inside the section) is silent too.
    transport(&mut world, json!({ "op": "play" }));
    tick(&mut world);
    transport(&mut world, json!({ "op": "stop" }));
    let out = state(&mut world);
    assert_eq!(out["sounds"], json!([]), "stopped: {out}");
    let (s, stopped) = voice_counts(&out);
    assert_eq!(stopped, s, "every started voice is stopped: {out}");
}

/// w10 (sprint-w9-rev-v06-lanes § 1 finding 2): Play-in-editor snapshots the level with the
/// preview lifted, so a video section's taken-over `VideoPlayer` (section clip, no autoplay,
/// external clock, volume 0) never reaches the play world: the play world gets the authored
/// player's exact scene text, and the edit world's preview carries on after the snapshot.
#[test]
fn play_in_editor_copies_the_authored_video_player_not_the_previewed_one() {
    use regia_framework::sequencer::{Binding, Channel, SequenceBindings, Track, VideoSection, VideoTarget};
    use regia_media::VideoPlayer;
    use regia_media::mux::PatternClip;
    use regia_scene::prelude::{Name, snapshot_scene};
    use regia_scene::spawn::SnapshotFilter;
    use regia_scene::spawn::UnregisteredComponents;

    let root = regia_test_utils::unique_temp_dir("seq-preview-pie-video");
    std::fs::create_dir_all(root.join("movies")).expect("dir");
    std::fs::write(root.join("Regia.toml"), "[project]\nname = \"g\"\n").expect("manifest");
    std::fs::write(
        root.join("movies/a.webm"),
        PatternClip::new(16, 16, 40, 25).to_webm(),
    )
    .expect("clip");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.finish_plugins();
    let world = app.world_mut();
    {
        let registry = world
            .get_resource_or_insert_with(regia_ecs::reflect::AppTypeRegistry::default)
            .clone();
        registry.write().register::<VideoPlayer>();
    }
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    crate::level_io::new_level(world);
    let guid = Guid::new_random();
    let own = VideoPlayer {
        source: "movies/own.webm".into(),
        autoplay: true,
        volume: 0.8,
        ..VideoPlayer::default()
    };
    let tv = world
        .spawn((
            Name::new("TV"),
            EntityGuid(guid),
            Transform::default(),
            own.clone(),
        ))
        .id();
    let mut sequence = tracks::new_sequence("intro", 6.0, 30.0).expect("new");
    let mut section = VideoSection::new("movies/a.webm", 1.0, 2.6);
    section.target = VideoTarget::Player("tv".into());
    sequence.tracks.push(Track {
        name: "screen".into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Video(vec![section]),
        enabled: true,
        locked: false,
    });
    world.spawn((
        SequencePlayer::new(sequence),
        SequenceBindings::default().with("tv", Binding::Entity(tv)),
    ));
    let player_text = |world: &mut World| -> Option<String> {
        let filter = SnapshotFilter {
            level: None,
            resources: Vec::new(),
        };
        let file = snapshot_scene(world, &filter).expect("snapshot");
        let entity = file.entities.into_iter().find(|e| e.guid == guid)?;
        entity
            .components
            .into_iter()
            .find(|(path, _)| path.ends_with("VideoPlayer"))
            .map(|(_, raw)| raw.get_ron().to_owned())
    };
    let authored = player_text(world).expect("the TV's player is level content");

    seq_preview(world, &json!({ "on": true, "time": 1.5 })).expect("preview");
    // w11 item 1c (ADR-7847): the TV shows the section in its session (frame 12 at 0.5 s), and its
    // authored player is never written (the w10 lane asserted the takeover here; corrected).
    let shown = |world: &World| {
        world
            .get::<regia_media::VideoPlayerState>(tv)
            .and_then(|s| s.core().frame_index())
    };
    assert_eq!(shown(world), Some(12), "the TV shows the section's picture");
    assert_eq!(
        world.get::<VideoPlayer>(tv),
        Some(&own),
        "the authored player stays as it is"
    );
    let mut session = crate::pie::start_play(world, None).expect("play");
    let play = session.app.world_mut();
    let mut query = play.query::<(&EntityGuid, &UnregisteredComponents)>();
    let copied = query.iter(play).find(|(g, _)| g.0 == guid).and_then(|(_, kept)| {
        kept.0
            .iter()
            .find(|(path, _)| path.ends_with("VideoPlayer"))
            .map(|(_, text)| text.clone())
    });
    assert_eq!(
        copied.as_deref(),
        Some(authored.as_str()),
        "the play world gets the authored player"
    );
    crate::pie::stop_play(session);
    let world = app.world_mut();
    assert_eq!(shown(world), Some(12), "the edit world's preview carries on");
    seq_preview(world, &json!({ "on": false })).expect("stop");
    assert_eq!(
        world.get::<VideoPlayer>(tv),
        Some(&own),
        "Stop gives the player back"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// w13 (ADR-8105): a locked track refuses every edit of its keys, sections, name or position with
/// `seq_track_locked`, through each path (track list, key from the scene, the curve and section
/// editors' edit path), and nothing changes; unlocked, the same edits apply.
#[test]
fn a_locked_track_refuses_edits_with_its_code() {
    let (mut world, _, _) = world_with_cube();
    world.insert_resource(UndoStack::new());
    seq_track_edit(
        &mut world,
        &json!({ "op": "add", "kind": "transform", "name": "move", "binding": "cube", "value": [1.0, 0.0, 0.0] }),
    )
    .expect("track");
    seq_track_edit(&mut world, &add_params("float")).expect("second track");
    let out = seq_track_edit(&mut world, &json!({ "op": "lock", "track": "move" })).expect("lock");
    assert_eq!(out["tracks"][1]["locked"], json!(true), "{out}");
    let before = bytes(&mut world);
    let depth = world.resource::<UndoStack>().undo_depth();
    for params in [
        json!({ "op": "rename", "track": "move", "new_name": "other" }),
        json!({ "op": "remove", "track": "move" }),
        json!({ "op": "move", "track": "move", "to": 0 }),
    ] {
        let err = seq_track_edit(&mut world, &params).expect_err("locked");
        assert_eq!(code(err), "seq_track_locked", "{params}");
    }
    // Key from the scene into the locked track.
    let err = seq_key(&mut world, &json!({ "binding": "cube" })).expect_err("locked key");
    assert_eq!(code(err), "seq_track_locked");
    // The curve and section editors' shared edit path.
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    panel.track = 1;
    let err = crate::seq_curve_panel::edit_track(&mut world, &mut panel, |channel| {
        if let Channel::Vec3(keys) = channel {
            keys[0].value = [9.0, 9.0, 9.0];
        }
        Ok(None)
    })
    .expect_err("locked curve");
    assert_eq!(err.code, "seq_track_locked");
    world.insert_resource(panel);
    assert_eq!(bytes(&mut world), before, "every refusal changed nothing");
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth,
        "and left no undo entry"
    );
    // Mute and duplicate still work on a locked track; unlocked, the rename applies.
    seq_track_edit(&mut world, &json!({ "op": "mute", "track": "move" })).expect("mute");
    let out = seq_track_edit(&mut world, &json!({ "op": "duplicate", "track": "move" })).expect("duplicate");
    assert_eq!(out["tracks"][2]["name"], json!("move.2"), "{out}");
    assert_eq!(out["tracks"][2]["locked"], json!(false));
    assert_eq!(out["tracks"][1]["muted"], json!(true));
    seq_track_edit(&mut world, &json!({ "op": "lock", "track": "move", "on": false })).expect("unlock");
    seq_track_edit(
        &mut world,
        &json!({ "op": "rename", "track": "move", "new_name": "other" }),
    )
    .expect("unlocked rename");
}

/// w13 (ADR-8105): solo is editor session state; with a track soloed the preview evaluates only it
/// (the other track's entity is left alone), unsolo brings the other back, and solo never changes
/// the saved sequence. Mute (the saved `enabled` switch) keeps a track out of the preview too.
#[test]
fn solo_mutes_the_other_tracks_in_preview() {
    let (mut world, _, cube) = world_with_cube();
    let lamp = world
        .spawn((Transform::default(), EntityGuid(Guid::new_random())))
        .id();
    seq_binding_edit(
        &mut world,
        &json!({ "op": "add", "binding": "lamp", "entity": lamp.to_bits() }),
    )
    .expect("bind");
    for (name, binding, value) in [
        ("move", "cube", [5.0, 0.0, 0.0]),
        ("lift", "lamp", [0.0, 3.0, 0.0]),
    ] {
        seq_track_edit(
            &mut world,
            &json!({ "op": "add", "kind": "transform", "name": name, "binding": binding, "value": value }),
        )
        .expect("track");
    }
    let saved = bytes(&mut world);
    let out = seq_track_edit(&mut world, &json!({ "op": "solo", "track": "move" })).expect("solo");
    assert_eq!(out["tracks"][0]["solo"], json!(true), "{out}");
    assert_eq!(out["tracks"][1]["solo"], json!(false));
    assert_eq!(bytes(&mut world), saved, "solo is not saved");
    seq_preview(&mut world, &json!({ "on": true })).expect("preview");
    let at = |world: &World, e: Entity| world.get::<Transform>(e).expect("t").translation.to_array();
    assert_eq!(at(&world, cube), [5.0, 0.0, 0.0], "the soloed track plays");
    assert_eq!(
        at(&world, lamp),
        [0.0, 0.0, 0.0],
        "the other track is muted by the solo"
    );
    seq_track_edit(&mut world, &json!({ "op": "solo", "track": "move", "on": false })).expect("unsolo");
    assert_eq!(
        at(&world, lamp),
        [0.0, 3.0, 0.0],
        "unsolo: every track plays again"
    );
    seq_preview(&mut world, &json!({ "on": false })).expect("stop");
    assert_eq!(at(&world, lamp), [0.0, 0.0, 0.0], "stop puts the value back");
    // Mute is saved and keeps the track out of the preview.
    seq_track_edit(&mut world, &json!({ "op": "mute", "track": "lift" })).expect("mute");
    assert_ne!(bytes(&mut world), saved, "mute is saved (`enabled: false`)");
    seq_preview(&mut world, &json!({ "on": true })).expect("preview");
    assert_eq!(at(&world, cube), [5.0, 0.0, 0.0]);
    assert_eq!(
        at(&world, lamp),
        [0.0, 0.0, 0.0],
        "a muted track is not evaluated"
    );
    seq_preview(&mut world, &json!({ "on": false })).expect("stop");
}

/// w13 (ADR-8105): event tracks are authored through `editor.seq_track_edit` (the dispatcher's
/// handler): add with a first key and payload, add, set and remove keys, refusals with their codes.
#[test]
fn event_keys_are_authored_through_seq_track_edit() {
    let (mut world, _, _) = world_with_cube();
    world.insert_resource(UndoStack::new());
    let out = call(
        &mut world,
        "editor.seq_track_edit",
        json!({ "op": "add", "kind": "event", "name": "cues", "event": "door", "payload": "open", "time": 1.0 }),
    );
    assert_eq!(out["tracks"][0]["kind"], json!("Event"), "{out}");
    assert_eq!(out["tracks"][0]["events"][0]["event"], json!("door"));
    assert_eq!(out["tracks"][0]["events"][0]["payload"], json!("open"));
    let out = call(
        &mut world,
        "editor.seq_track_edit",
        json!({ "op": "event_key", "track": "cues", "action": "add", "time": 0.5, "event": "bell" }),
    );
    assert_eq!(out["key"], json!(0), "{out}");
    let out = call(
        &mut world,
        "editor.seq_track_edit",
        json!({ "op": "event_key", "track": "cues", "action": "set", "key": 0, "time": 2.0, "event": "late", "payload": "x" }),
    );
    assert_eq!(
        out["key"],
        json!(1),
        "a retimed key moves to its time order: {out}"
    );
    let out = call(
        &mut world,
        "editor.seq_track_edit",
        json!({ "op": "event_key", "track": "cues", "action": "remove", "key": 0 }),
    );
    assert_eq!(
        out["tracks"][0]["events"].as_array().map(Vec::len),
        Some(1),
        "{out}"
    );
    let text = bytes(&mut world);
    assert!(text.contains("Event(["), "{text}");
    assert!(text.contains("event: \"late\""), "{text}");
    for (params, want) in [
        (
            json!({ "op": "event_key", "track": "cues", "action": "remove", "key": 0 }),
            "seq_event_invalid",
        ),
        (
            json!({ "op": "event_key", "track": "cues", "action": "remove", "key": 7 }),
            "seq_event_bad_index",
        ),
        (
            json!({ "op": "event_key", "track": "cues", "action": "add", "time": -1.0, "event": "x" }),
            "seq_event_invalid",
        ),
    ] {
        let err = seq_track_edit(&mut world, &params).expect_err("refused");
        assert_eq!(code(err), want, "{params}");
    }
    assert_eq!(bytes(&mut world), text, "refusals changed nothing");
    // Undo takes the edits back one by one.
    assert!(undo_world(&mut world));
    assert!(bytes(&mut world).contains("event: \"door\""));
}

/// w13 (ADR-8105) keyboard path: Tab to a track row, Shift+F10 opens its menu, Tab walks to Lock
/// and Enter locks the track (one undo entry); the row's name says so; Shift+F10, Solo works the
/// same way (session state, no undo entry).
#[test]
fn the_track_menu_works_from_the_keyboard() {
    use crate::canvas_keys::test_input::{frame, key, labels_of, tab_to};
    use egui::{Key, Modifiers};
    let (mut world, player, _) = world_with_cube();
    world.insert_resource(UndoStack::new());
    seq_track_edit(
        &mut world,
        &json!({ "op": "add", "kind": "transform", "name": "move", "binding": "cube" }),
    )
    .expect("track");
    let depth = world.resource::<UndoStack>().undo_depth();
    let ctx = egui::Context::default();
    let size = [900.0, 1400.0];
    let draw = |ui: &mut egui::Ui, world: &mut World| {
        egui::CentralPanel::default().show(ui, |ui| crate::seq_curve_panel::seq_curve_panel(ui, world));
    };
    frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
    // Open the authoring rows from the keyboard.
    tab_to(&ctx, size, "Sequence authoring", false, 120, |ui| {
        draw(ui, &mut world)
    });
    frame(&ctx, size, vec![key(Key::Enter, Modifiers::NONE)], |ui| {
        draw(ui, &mut world)
    });
    frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
    let row = "move (Vec3)";
    tab_to(&ctx, size, row, false, 120, |ui| draw(ui, &mut world));
    let tree = frame(&ctx, size, vec![key(Key::F10, Modifiers::SHIFT)], |ui| {
        draw(ui, &mut world)
    });
    let buttons = labels_of(&tree, egui::accesskit::Role::Button);
    for item in ["Duplicate", "Delete", "Lock", "Mute", "Solo", "Select all keys"] {
        assert!(
            buttons.iter().any(|b| b == item),
            "Shift+F10 opened the row menu: {buttons:?}"
        );
    }
    tab_to(&ctx, size, "Lock", false, 12, |ui| draw(ui, &mut world));
    frame(&ctx, size, vec![key(Key::Enter, Modifiers::NONE)], |ui| {
        draw(ui, &mut world)
    });
    let tree = frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
    let locked = world
        .get::<SequencePlayer>(player)
        .expect("player")
        .sequence()
        .tracks[0]
        .locked;
    assert!(locked, "Enter on Lock locked the track");
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "one undo entry"
    );
    let named: Vec<String> = tree
        .nodes
        .iter()
        .filter_map(|(_, node)| node.label().map(str::to_string))
        .collect();
    assert!(named.iter().any(|r| r == "move (Vec3), locked"), "{named:?}");
    // Solo from the keyboard too.
    tab_to(&ctx, size, "move (Vec3), locked", false, 120, |ui| {
        draw(ui, &mut world)
    });
    frame(&ctx, size, vec![key(Key::F10, Modifiers::SHIFT)], |ui| {
        draw(ui, &mut world)
    });
    tab_to(&ctx, size, "Solo", false, 12, |ui| draw(ui, &mut world));
    frame(&ctx, size, vec![key(Key::Enter, Modifiers::NONE)], |ui| {
        draw(ui, &mut world)
    });
    frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
    assert!(
        soloed(&world, player).contains("move"),
        "Enter on Solo soloed the track"
    );
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "solo is not an undo entry"
    );
}

/// A project with a 40-frame pattern clip at `movies/a.webm`, a level, a TV (guid, authored
/// `VideoPlayer` on `movies/own.webm`) bound as `tv`, and a player whose video section (1..2.6 s)
/// targets the TV's player. Returns the app, the project root, the open level, the TV's guid and
/// its authored player.
fn video_preview_app(
    name: &str,
) -> (
    App,
    std::path::PathBuf,
    crate::level_io::OpenLevel,
    Guid,
    regia_media::VideoPlayer,
) {
    use regia_framework::sequencer::{Binding, Channel, SequenceBindings, Track, VideoSection, VideoTarget};
    use regia_media::VideoPlayer;
    use regia_media::mux::PatternClip;
    use regia_scene::prelude::Name;

    let root = regia_test_utils::unique_temp_dir(name);
    std::fs::create_dir_all(root.join("movies")).expect("dir");
    std::fs::write(root.join("Regia.toml"), "[project]\nname = \"g\"\n").expect("manifest");
    std::fs::write(
        root.join("movies/a.webm"),
        PatternClip::new(16, 16, 40, 25).to_webm(),
    )
    .expect("clip");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.finish_plugins();
    let world = app.world_mut();
    {
        let registry = world
            .get_resource_or_insert_with(regia_ecs::reflect::AppTypeRegistry::default)
            .clone();
        registry.write().register::<VideoPlayer>();
    }
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    let mut open = crate::level_io::new_level(world);
    open.path = Some(root.join("main.level.ron"));
    world.insert_resource(open.clone());
    let guid = Guid::new_random();
    let own = VideoPlayer {
        source: "movies/own.webm".into(),
        autoplay: true,
        volume: 0.8,
        ..VideoPlayer::default()
    };
    let tv = world
        .spawn((
            Name::new("TV"),
            EntityGuid(guid),
            Transform::default(),
            own.clone(),
        ))
        .id();
    let mut sequence = tracks::new_sequence("intro", 6.0, 30.0).expect("new");
    let mut section = VideoSection::new("movies/a.webm", 1.0, 2.6);
    section.target = VideoTarget::Player("tv".into());
    sequence.tracks.push(Track {
        name: "screen".into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Video(vec![section]),
        enabled: true,
        locked: false,
    });
    world.spawn((
        SequencePlayer::new(sequence),
        SequenceBindings::default().with("tv", Binding::Entity(tv)),
    ));
    (app, root, open, guid, own)
}

/// The TV by its guid.
fn entity_of(world: &mut World, guid: Guid) -> Option<Entity> {
    let mut query = world.query::<(Entity, &EntityGuid)>();
    query.iter(world).find(|(_, g)| g.0 == guid).map(|(e, _)| e)
}

/// w11 item 1c (sprint-w9-rev-v06-lanes § 1 finding 5): a delete or a duplicate of a bound entity
/// while its video player previews records the authored `VideoPlayer`, never the preview's: undo
/// after the preview stops leaves no preview state (the section clip, no autoplay, the external
/// clock, volume 0) in the level bytes, and the copy carries the authored player too.
#[test]
fn undo_and_duplicate_during_a_video_preview_keep_the_preview_out_of_the_level_bytes() {
    use regia_media::VideoPlayer;

    let (mut app, root, open, guid, own) = video_preview_app("seq-preview-video-undo");
    let world = app.world_mut();
    let tv = entity_of(world, guid).expect("tv");
    seq_preview(world, &json!({ "on": true, "time": 1.5 })).expect("preview");
    // A duplicate made while the preview shows the section.
    let copies =
        crate::hierarchy_ops::duplicate_entities(world, &[tv], regia_core::math::Vec3::X).expect("duplicate");
    assert_eq!(
        world.get::<VideoPlayer>(copies[0]),
        Some(&own),
        "the copy carries the authored player"
    );
    // A delete made while the preview shows the section, then Stop, then undo of the delete.
    crate::hierarchy_ops::delete_entities(world, &[tv]).expect("delete");
    seq_preview(world, &json!({ "on": false })).expect("stop");
    assert!(undo_world(world), "undo the delete");
    let back = entity_of(world, guid).expect("the TV is back");
    assert_eq!(
        world.get::<VideoPlayer>(back),
        Some(&own),
        "undo restores the authored player"
    );
    let bytes = crate::level_io::snapshot_saved_level(world, open.id)
        .expect("snapshot")
        .to_ron_string()
        .expect("ron");
    assert!(
        !bytes.contains("movies/a.webm"),
        "no preview player in the level bytes: {bytes}"
    );
    assert!(bytes.contains("movies/own.webm"), "{bytes}");
    let _ = std::fs::remove_dir_all(&root);
}

/// w11 item 1d (sprint-w9-rev-v06-lanes § 1 finding 6): a level save, an autosave's snapshot (the
/// same lifted snapshot), an asset-watch pass on the open level and a Play start while a video
/// section previews never close and re-open the previewed video: one open for the whole run
/// (`video_opens`), and the picture stays on the section's frame.
#[test]
fn save_autosave_play_and_asset_watch_do_not_reopen_the_previewed_video() {
    use regia_media::VideoPlayerState;

    let (mut app, root, open, guid, own) = video_preview_app("seq-preview-video-reopen");
    let world = app.world_mut();
    let tv = entity_of(world, guid).expect("tv");
    let opens = |out: &Value| out["video_opens"].as_u64();
    let out = seq_preview(world, &json!({ "on": true, "time": 1.5 })).expect("preview");
    assert_eq!(opens(&out), Some(1), "{out}");
    let frame = |world: &World| {
        world
            .get::<VideoPlayerState>(tv)
            .and_then(|s| s.core().frame_index())
    };
    // 0.5 s into the section: frame 12 (480 ms) of the 25 fps clip.
    assert_eq!(frame(world), Some(12));

    let path = open.path.clone().expect("path");
    crate::level_io::save_level_file(world, &open, &path).expect("save");
    // The autosave's level write is this lifted snapshot (`doc_autosave::level_write`).
    with_preview_lifted(world, |world| {
        crate::level_io::snapshot_saved_level(world, open.id).map(|_| ())
    })
    .expect("autosave snapshot");
    crate::asset_watch::on_level_changed(world, &path);
    let session = crate::pie::start_play(world, None).expect("play");
    crate::pie::stop_play(session);
    let world = app.world_mut();
    let out = seq_preview(world, &json!({ "on": true })).expect("state");
    assert_eq!(opens(&out), Some(1), "no re-open: {out}");
    assert_eq!(frame(world), Some(12), "the section's picture stays");
    let saved = std::fs::read_to_string(&path).expect("saved level");
    assert!(!saved.contains("movies/a.webm"), "{saved}");
    seq_preview(world, &json!({ "on": false })).expect("stop");
    assert_eq!(world.get::<regia_media::VideoPlayer>(tv), Some(&own));
    let _ = std::fs::remove_dir_all(&root);
}

/// The headless output, a 4 s `chime` and a 0.6 s `blip`, for the voice tests.
fn sound_world() -> World {
    use regia_audio::{Audio, AudioSettings, SoundAsset, SoundAssets, SoundClip};

    let mut world = World::new();
    world.insert_resource(Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    }));
    let mut sounds = SoundAssets::default();
    let chime = SoundClip::new(vec![0.1; 48_000 * 4], 48_000).expect("clip");
    sounds.add("chime", SoundAsset::from_clip(&chime));
    let blip = SoundClip::new(vec![0.1; 28_800], 48_000).expect("clip");
    sounds.add("blip", SoundAsset::from_clip(&blip));
    world.insert_resource(sounds);
    world
}

/// w11 item 1a (sprint-w9-rev-v06-lanes § 1 finding 3): with no `Audio` listener in the edit
/// world, a spatial preview sound is heard from the editor camera, not the world origin: its pan
/// and level are the runtime spatialization against a listener at the camera's eye facing its
/// forward.
#[test]
fn a_spatial_preview_sound_without_a_listener_is_heard_from_the_editor_camera() {
    use regia_audio::ListenerFrame;
    use regia_core::math::{Quat, Vec3};
    use regia_framework::sequencer::audio::spatial_frame;
    use regia_framework::sequencer::{AudioSection, AudioSpatial, Channel, SoundSource, Track};

    let mut world = sound_world();
    let camera = crate::viewport::EditorCamera {
        target: Vec3::new(60.0, 0.0, 0.0),
        distance: 5.0,
        yaw: 0.0,
        pitch: 0.0,
        ..crate::viewport::EditorCamera::default()
    };
    world.insert_resource(camera.clone());
    let at = Vec3::new(58.0, 0.0, 0.0);
    let mut section = AudioSection::new("chime", 1.0, 3.0);
    let spatial = AudioSpatial::new(SoundSource::Position(at.to_array()));
    section.spatial = Some(spatial.clone());
    let mut sequence = tracks::new_sequence("intro", 6.0, 30.0).expect("new");
    sequence.tracks.push(Track {
        name: "door".into(),
        target: String::new(),
        property: String::new(),
        keys: Channel::Audio(vec![section]),
        enabled: true,
        locked: false,
    });
    world.spawn(SequencePlayer::new(sequence));
    let out = seq_preview(&mut world, &json!({ "on": true, "time": 1.5 })).expect("preview");
    let door = out["sounds"][0].clone();
    let pan = door["pan"].as_f64().expect("pan") as f32;
    let near = spatial_frame(
        &spatial,
        at,
        Quat::IDENTITY,
        &[ListenerFrame::looking_at(camera.eye(), camera.forward())],
    );
    let origin = spatial_frame(&spatial, at, Quat::IDENTITY, &[]);
    assert!(
        (near.pan - origin.pan).abs() > 0.1,
        "the fixture tells the two listeners apart: {near:?} vs {origin:?}"
    );
    assert!(
        (pan - near.pan).abs() < 1.0e-4,
        "heard from the camera (pan {}), not the origin (pan {}): {door}",
        near.pan,
        origin.pan
    );
    let db = door["volume_db"].as_f64().expect("db") as f32;
    let expected = regia_audio::gain_to_db(near.gain).max(-100.0);
    assert!(
        (db - expected).abs() < 1.0e-3,
        "level from the camera: {db} vs {expected}: {door}"
    );
}

/// w11 item 1b (sprint-w9-rev-v06-lanes § 1 finding 4): reverse play does not restart a voice on
/// every tick (the preview is silent while the transport runs backward); forward ticks longer
/// than a frame (a slow frame) keep the voice; a voice whose sound ended is no longer listed as
/// playing. Counted on the headless output.
#[test]
fn reverse_play_and_slow_frames_do_not_restart_voices_and_an_ended_sound_is_not_playing() {
    use regia_framework::sequencer::{AudioSection, Channel, Track};

    let mut world = sound_world();
    let mut sequence = tracks::new_sequence("intro", 8.0, 30.0).expect("new");
    for (name, sound) in [("music", "chime"), ("hit", "blip")] {
        sequence.tracks.push(Track {
            name: name.into(),
            target: String::new(),
            property: String::new(),
            keys: Channel::Audio(vec![AudioSection::new(sound, 1.0, 6.0)]),
            enabled: true,
            locked: false,
        });
    }
    world.spawn(SequencePlayer::new(sequence));
    let transport = |world: &mut World, params: Value| call(world, "editor.seq_transport", params);
    let state = |world: &mut World| seq_preview(world, &json!({ "on": true })).expect("state");
    let playing = |out: &Value, track: &str| -> Option<bool> {
        out["sounds"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["track"] == track))
            .and_then(|s| s["playing"].as_bool())
    };

    // Forward, slow frames (0.4 s per tick): one start per voice, kept.
    let out = seq_preview(&mut world, &json!({ "on": true, "time": 1.1 })).expect("preview");
    assert_eq!(voice_counts(&out), (Some(2), Some(0)), "{out}");
    transport(&mut world, json!({ "op": "play" }));
    transport(&mut world, json!({ "op": "tick", "dt": 0.4 }));
    let out = state(&mut world);
    assert_eq!(
        voice_counts(&out),
        (Some(2), Some(0)),
        "a slow frame keeps the voices: {out}"
    );
    assert_eq!(playing(&out, "music"), Some(true), "{out}");
    // The 0.6 s `blip` (from 1 s) ended at 1.6 s: not playing at 1.9 s (and not restarted).
    transport(&mut world, json!({ "op": "tick", "dt": 0.4 }));
    let out = state(&mut world);
    assert_eq!(playing(&out, "hit"), Some(false), "the blip ended: {out}");
    assert_eq!(playing(&out, "music"), Some(true), "{out}");
    assert_eq!(
        out["voices"]["started"].as_u64(),
        Some(2),
        "nothing restarted: {out}"
    );

    // Reverse: no restart on any tick; the voices stop and stay stopped, also through a re-apply
    // at the same time (the state call).
    transport(&mut world, json!({ "op": "reverse" }));
    for _ in 0..3 {
        transport(&mut world, json!({ "op": "tick", "dt": 0.1 }));
        state(&mut world);
    }
    let out = state(&mut world);
    assert_eq!(
        out["voices"]["started"].as_u64(),
        Some(2),
        "reverse play starts no voice: {out}"
    );
    let (started, stopped) = voice_counts(&out);
    assert_eq!(started, stopped, "every voice is stopped in reverse: {out}");
    // Forward again: the music starts once at the playhead and plays on.
    transport(&mut world, json!({ "op": "play" }));
    for _ in 0..3 {
        transport(&mut world, json!({ "op": "tick", "dt": 0.1 }));
    }
    let out = state(&mut world);
    assert_eq!(out["voices"]["started"].as_u64(), Some(3), "{out}");
    assert_eq!(playing(&out, "music"), Some(true), "{out}");
}

/// Lane v06-editor-undo-agent-routes (ADR-8583, smoke bug 10): the player `editor.seq_new` (and
/// `editor.seq_curve_open`) spawns is the Sequence Curves panel's preview of a `.seq.ron` document,
/// saved to its own file. It is editor-only: a level save stays byte-identical, the save filter,
/// the glTF export and Live link leave it out (`EditorOnly`), even once it has a guid or is moved
/// under level content, and it is no undo entry (nothing the level saves changed).
#[test]
fn seq_new_preview_player_is_not_saved() {
    let root = regia_test_utils::unique_temp_dir("seq_tracks_new_not_saved");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>();
    app.finish_plugins();
    let world = app.world_mut();
    world.insert_resource(EditorProject {
        root: Some(root.clone()),
    });
    let open = crate::level_io::new_level(world);
    let guid = Guid::new_random();
    let mut spawn = SpawnEntities::new_entity(world, guid, "Cube", Transform::default()).expect("spawn");
    spawn.apply(world);
    let level = root.join("room.level.ron");
    crate::level_io::save_level_file(world, &open, &level).expect("save");
    let saved = std::fs::read(&level).expect("read");
    let depth = world.resource::<UndoStack>().undo_depth();

    let out = seq_new(world, &json!({ "path": "cine/shot.seq.ron" })).expect("new");
    let player = out["player"]
        .as_u64()
        .and_then(Entity::try_from_bits)
        .expect("player id");
    assert!(
        world.get::<crate::editor_only::EditorOnly>(player).is_some(),
        "the preview player is marked editor-only"
    );
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth,
        "no undo entry: the level did not change"
    );
    crate::level_io::save_level_file(world, &open, &level).expect("save after seq_new");
    assert_eq!(
        std::fs::read(&level).expect("read"),
        saved,
        "the level save is byte-identical"
    );
    // The save gave it a guid; every editor-only filter (export, Live link) now knows it.
    let player_guid = world
        .get::<EntityGuid>(player)
        .map(|g| g.0)
        .expect("a guid after the save");
    assert!(crate::editor_only::editor_only_guids(world).contains(&player_guid));
    // Moved under level content (an Outliner drag), it still never reaches the file.
    let cube = {
        let mut query = world.query::<(Entity, &EntityGuid)>();
        query
            .iter(world)
            .find(|(_, g)| g.0 == guid)
            .map(|(e, _)| e)
            .expect("cube")
    };
    world.entity_mut(cube).add_child(player);
    world
        .entity_mut(player)
        .insert(regia_scene::prelude::LevelMember::from_level(open.id));
    let text = crate::level_io::snapshot_saved_level(world, open.id)
        .expect("snapshot")
        .to_ron_string()
        .expect("ron");
    assert!(
        !text.contains("sequence:cine/shot.seq.ron") && !text.contains(&player_guid.to_string()),
        "the preview player is not in the saved level: {text}"
    );
    // `editor.seq_curve_open` spawns the same kind of player.
    let mut panel = world.remove_resource::<SeqCurvePanel>().unwrap_or_default();
    let reopened =
        crate::seq_curve_panel::open_sequence(world, &mut panel, "cine/shot.seq.ron").expect("open");
    world.insert_resource(panel);
    assert!(world.get::<crate::editor_only::EditorOnly>(reopened).is_some());
}

// ---- v06-seq-preview-fidelity (ADR-8591): pitch-preserving stretch, Doppler, occlusion and
// non-bound video targets in the edit-world preview ----

/// A 0.5-amplitude sine of `hz` lasting `secs` at 48 kHz.
fn sine_asset(hz: f32, secs: f32) -> regia_audio::SoundAsset {
    let samples: Vec<f32> = (0..(secs * 48_000.0) as usize)
        .map(|i| (std::f32::consts::TAU * hz * i as f32 / 48_000.0).sin() * 0.5)
        .collect();
    regia_audio::SoundAsset::from_clip(&regia_audio::SoundClip::new(samples, 48_000).expect("clip"))
}

/// The frequency of the strongest bin of a Hann-windowed DFT of `x` (bins up to `max_hz`), and the
/// bin width.
fn dominant_hz(x: &[f32], rate: f32, max_hz: f32) -> (f32, f32) {
    let n = x.len();
    let bin = rate / n as f32;
    let top = ((max_hz / bin) as usize).min(n / 2);
    let windowed: Vec<f64> = x
        .iter()
        .enumerate()
        .map(|(i, s)| f64::from(*s) * (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos()))
        .collect();
    let mut best = (0usize, 0.0f64);
    for k in 1..top {
        let w = std::f64::consts::TAU * k as f64 / n as f64;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, v) in windowed.iter().enumerate() {
            re += v * (w * i as f64).cos();
            im -= v * (w * i as f64).sin();
        }
        let m = re * re + im * im;
        if m > best.1 {
            best = (k, m);
        }
    }
    (best.0 as f32 * bin, bin)
}

/// An edit world with the headless output in a running mixdown and `sounds`.
fn mixdown_world(sounds: &[(&str, regia_audio::SoundAsset)]) -> World {
    use regia_audio::{Audio, AudioSettings, MixdownSettings, SoundAssets};

    let mut world = World::new();
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    audio.begin_mixdown(&MixdownSettings::default()).expect("mixdown");
    world.insert_resource(audio);
    let mut library = SoundAssets::default();
    for (name, asset) in sounds {
        library.add(*name, asset.clone());
    }
    world.insert_resource(library);
    world
}

/// `frames` blocks of 800 frames of the edit world's mixdown (left plus right).
fn render_preview_mix(world: &mut World, blocks: usize) -> Vec<f32> {
    let mut out = Vec::new();
    for _ in 0..blocks {
        let mut audio = world.resource_mut::<regia_audio::Audio>();
        audio.tick(1.0 / 60.0);
        let mix = audio.render_mixdown(800).expect("block").mix;
        out.extend(mix.as_chunks::<2>().0.iter().map(|[l, r]| l + r));
    }
    out
}

/// An audio track named `name` with `sections`.
fn sound_track(
    name: &str,
    sections: Vec<regia_framework::sequencer::AudioSection>,
) -> regia_framework::sequencer::Track {
    regia_framework::sequencer::Track {
        name: name.into(),
        target: String::new(),
        property: String::new(),
        keys: regia_framework::sequencer::Channel::Audio(sections),
        enabled: true,
        locked: false,
    }
}

/// v06-seq-preview-fidelity item 1: a pitch-preserving section at rate 2 plays its sound
/// pitch-preserved in the edit preview (the runtime's WSOLA stretch, `regia_audio::stretch_asset`
/// over the section's range at the voice rate), not by varispeed: the dominant frequency of a
/// 440 Hz sine stays within one FFT bin of 440 Hz, where the varispeed control doubles it. Sample
/// buffers of the headless mixdown, no wall clock.
#[test]
fn preview_of_a_preserve_pitch_section_keeps_pitch_at_rate_2() {
    use regia_framework::sequencer::{AudioSection, TimeScaleMode};

    let render = |mode: TimeScaleMode| -> (f32, f32, Value) {
        let mut world = mixdown_world(&[("tone", sine_asset(440.0, 1.0))]);
        // 0.12 s of sequence at rate 2: 0.24 s of the sound (at most `STRETCH_INLINE_SECS`, so the
        // stretch renders on the apply that needs it, as at runtime).
        let mut section = AudioSection::new("tone", 1.0, 1.12);
        section.time_scale = 2.0;
        section.time_scale_mode = mode;
        let mut sequence = tracks::new_sequence("intro", 3.0, 30.0).expect("new");
        sequence.tracks.push(sound_track("voice", vec![section]));
        world.spawn(SequencePlayer::new(sequence));
        let out = seq_preview(&mut world, &json!({ "on": true, "time": 1.0 })).expect("preview");
        let mix = render_preview_mix(&mut world, 7);
        let (hz, bin) = dominant_hz(&mix[512..512 + 4096], 48_000.0, 2_000.0);
        (hz, bin, out)
    };
    let (vari, bin, out) = render(TimeScaleMode::Varispeed);
    assert!(
        (vari - 880.0).abs() <= bin,
        "the varispeed control doubles the pitch ({vari} Hz, bin {bin} Hz): {out}"
    );
    let (kept, bin, out) = render(TimeScaleMode::PreservePitch);
    assert!(
        (kept - 440.0).abs() <= bin,
        "pitch-preserving at rate 2 keeps 440 Hz within one bin ({bin} Hz): got {kept} Hz: {out}"
    );
    assert_eq!(out["sounds"][0]["stretched"], true, "{out}");
    assert_eq!(out["sounds"][0]["playing"], true, "{out}");
}

/// A sequence of `secs` whose `car` binding moves along +X from `from` at `speed` m/s (a linear
/// `transform.position` track), carrying `tone` from a spatial section bound to it.
fn passing_car_sequence(
    from: regia_core::math::Vec3,
    speed: f32,
    secs: f32,
    spatial: regia_framework::sequencer::AudioSpatial,
) -> regia_framework::sequencer::Sequence {
    use regia_framework::sequencer::{AudioSection, Channel, Ease, Track, Vec3Key};

    let mut sequence = tracks::new_sequence("pass", secs, 30.0).expect("new");
    let end = from + regia_core::math::Vec3::X * speed * secs;
    sequence.tracks.push(Track {
        name: "car".into(),
        target: "car".into(),
        property: "transform.position".into(),
        keys: Channel::Vec3(vec![
            Vec3Key::new(0.0, from.to_array(), Ease::Linear),
            Vec3Key::new(secs, end.to_array(), Ease::Linear),
        ]),
        enabled: true,
        locked: false,
    });
    let mut section = AudioSection::new("tone", 0.0, secs);
    section.spatial = Some(spatial);
    sequence.tracks.push(sound_track("engine", vec![section]));
    sequence
}

/// v06-seq-preview-fidelity item 2a: a bound emitter driving toward the editor camera is heard
/// Doppler-shifted in the preview, by the runtime's own number: the same sequence played by the
/// runtime sequencer (`SequencerPlugin`, a listener at the camera's eye facing its forward, fixed
/// 60 Hz steps) reports the same Doppler ratio for its voice at the same clock, and the preview's
/// voice pitch is that ratio. Fixed steps and pure numbers, no wall clock.
#[test]
fn preview_doppler_shifts_an_approaching_emitter() {
    use regia_app::prelude::TimeUpdateStrategy;
    use regia_audio::{Audio, AudioSettings, ListenerFrame, SoundAssets};
    use regia_core::math::Vec3;
    use regia_framework::sequencer::{AudioSpatial, Binding, SequenceBindings, SequenceVoices, SoundSource};

    let camera = crate::viewport::EditorCamera {
        target: Vec3::ZERO,
        distance: 5.0,
        yaw: 0.0,
        pitch: 0.0,
        ..crate::viewport::EditorCamera::default()
    };
    let eye = camera.eye();
    // From 60 m down +X of the eye, driving toward it (and past it) at 30 m/s.
    let from = eye - Vec3::X * 60.0;
    let mut spatial = AudioSpatial::new(SoundSource::Binding("car".into()));
    spatial.doppler = 1.0;
    spatial.max_distance = 500.0;
    let sequence = passing_car_sequence(from, 30.0, 4.0, spatial);

    // The runtime: 61 fixed steps of 1/60 s.
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SequencerPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_nanos(16_666_667),
        ));
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    audio.set_listener(ListenerFrame::looking_at(eye, camera.forward()));
    app.insert_resource(audio);
    let mut sounds = SoundAssets::default();
    sounds.add("tone", sine_asset(440.0, 6.0));
    app.insert_resource(sounds);
    let car = app.world_mut().spawn(Transform::default()).id();
    let mut runtime_player = SequencePlayer::new(sequence.clone());
    runtime_player.play();
    let runtime = app
        .world_mut()
        .spawn((
            runtime_player,
            SequenceBindings::default().with("car", Binding::Entity(car)),
        ))
        .id();
    for _ in 0..61 {
        app.update();
    }
    let clock = app
        .world()
        .get::<SequencePlayer>(runtime)
        .map(SequencePlayer::time)
        .expect("player");
    let voices = app.world().resource::<SequenceVoices>().voices(runtime);
    let runtime_doppler = voices.first().map(|(_, v)| v.doppler).expect("a runtime voice");
    assert!(
        runtime_doppler > 1.05,
        "the fixture approaches: runtime ratio {runtime_doppler} at {clock} s"
    );

    // The edit preview at the same clock, heard from the editor camera.
    let mut world = sound_world();
    world
        .resource_mut::<SoundAssets>()
        .add("tone", sine_asset(440.0, 6.0));
    world.insert_resource(camera);
    let car = world.spawn(Transform::default()).id();
    world.spawn((
        SequencePlayer::new(sequence),
        SequenceBindings::default().with("car", Binding::Entity(car)),
    ));
    let out = seq_preview(&mut world, &json!({ "on": true, "time": clock })).expect("preview");
    let engine = out["sounds"][0].clone();
    let doppler = engine["doppler"].as_f64().map(|d| d as f32);
    assert!(
        doppler.is_some_and(|d| (d - runtime_doppler).abs() < 1.0e-3),
        "the preview's Doppler ratio is the runtime's ({runtime_doppler}): {engine}"
    );
    let pitch = engine["pitch"].as_f64().map(|d| d as f32);
    assert!(
        pitch.is_some_and(|p| (p - runtime_doppler).abs() < 1.0e-3),
        "the voice plays at the Doppler pitch (rate 1): {engine}"
    );
}

/// v06-seq-preview-fidelity item 2b: an occluding spatial section behind a wall collider (between
/// the editor camera and the emitter) is attenuated in the preview by the runtime's own number:
/// the runtime sequencer with the physics occlusion provider (`regia::audio_occlusion`) over the
/// same wall, settled over fixed steps, reports the same voice volume and occlusion amount for the
/// same clock; the same section with the wall gone is louder by the tuning's gain. Fixed steps and
/// pure numbers, no wall clock.
#[test]
fn preview_occlusion_attenuates_behind_a_wall() {
    use regia_app::prelude::{FixedTime, PostUpdate, TimeUpdateStrategy};
    use regia_audio::{Audio, AudioOcclusion, AudioSettings, ListenerFrame, SoundAssets};
    use regia_core::math::Vec3;
    use regia_framework::sequencer::{AudioSection, AudioSpatial, SequenceVoices, SoundSource};
    use regia_physics::{Collider, ColliderShape, RigidBody};

    let camera = crate::viewport::EditorCamera {
        target: Vec3::ZERO,
        distance: 5.0,
        yaw: 0.0,
        pitch: 0.0,
        ..crate::viewport::EditorCamera::default()
    };
    let eye = camera.eye();
    let at = eye + camera.forward() * 12.0;
    let wall_at = eye + camera.forward() * 6.0;
    let wall = || {
        (
            Transform::from_translation(wall_at),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(4.0, 4.0, 4.0) * 0.5,
                },
                ..Collider::default()
            },
        )
    };
    let mut spatial = AudioSpatial::new(SoundSource::Position(at.to_array()));
    spatial.occlusion = true;
    spatial.max_distance = 100.0;
    let mut section = AudioSection::new("tone", 0.0, 4.0);
    section.spatial = Some(spatial);
    let mut sequence = tracks::new_sequence("wall", 4.0, 30.0).expect("new");
    sequence.tracks.push(sound_track("hum", vec![section]));

    // The runtime: sequencer + physics + the occlusion provider, 90 fixed steps (the colliders
    // sync, the rays are answered and the smoothing settles).
    let mut app = App::new();
    let step = std::time::Duration::from_nanos(16_666_667);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.add_plugins((MinimalPlugins, SequencerPlugin, regia_physics::PhysicsPlugin));
    app.init_resource::<regia::audio_occlusion::OcclusionRayStats>();
    // The audio sync is not running here: queue the sequencer's rays as it would
    // (`AudioOcclusion::queue`), then let the physics provider answer them.
    app.add_systems(
        PostUpdate,
        (
            |mut occlusion: ResMut<AudioOcclusion>| occlusion.queue(&[]),
            regia::audio_occlusion::physics_audio_occlusion,
        )
            .chain()
            .run_if(regia_ecs::prelude::resource_exists::<regia_physics::PhysicsWorld>),
    );
    let mut audio = Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    audio.set_listener(ListenerFrame::looking_at(eye, camera.forward()));
    app.insert_resource(audio);
    app.insert_resource(AudioOcclusion::default());
    let mut sounds = SoundAssets::default();
    sounds.add("tone", sine_asset(440.0, 6.0));
    app.insert_resource(sounds);
    app.world_mut().spawn(wall());
    let mut runtime_player = SequencePlayer::new(sequence.clone());
    runtime_player.play();
    let runtime = app.world_mut().spawn(runtime_player).id();
    for _ in 0..90 {
        app.update();
    }
    let clock = app
        .world()
        .get::<SequencePlayer>(runtime)
        .map(SequencePlayer::time)
        .expect("player");
    let voices = app.world().resource::<SequenceVoices>().voices(runtime);
    let runtime_voice = voices.first().map(|(_, v)| *v).expect("a runtime voice");
    assert!(
        runtime_voice.occlusion > 0.5,
        "the runtime hears the wall: {runtime_voice:?}"
    );

    // The edit preview at the same clock, the wall in the edit world, heard from the camera.
    let preview = |with_wall: bool| -> Value {
        let mut world = sound_world();
        world
            .resource_mut::<SoundAssets>()
            .add("tone", sine_asset(440.0, 6.0));
        world.insert_resource(camera.clone());
        if with_wall {
            world.spawn(wall());
        }
        world.spawn(SequencePlayer::new(sequence.clone()));
        let out = seq_preview(&mut world, &json!({ "on": true, "time": clock })).expect("preview");
        out["sounds"][0].clone()
    };
    let behind = preview(true);
    let open = preview(false);
    let db = |s: &Value| s["volume_db"].as_f64().unwrap_or(f64::NAN) as f32;
    assert!(
        (db(&behind) - runtime_voice.volume_db).abs() < 0.01,
        "behind the wall: the runtime's volume {} dB, the preview's {}: {behind}",
        runtime_voice.volume_db,
        db(&behind)
    );
    let amount = behind["occlusion"].as_f64().map(|a| a as f32);
    assert!(
        amount.is_some_and(|a| (a - runtime_voice.occlusion).abs() < 1.0e-3),
        "the runtime's occlusion amount {}: {behind}",
        runtime_voice.occlusion
    );
    let tuning = regia_audio::OcclusionTuning::default();
    let drop = regia_audio::gain_to_db(tuning.gain(tuning.amount_for_obstacles(1)));
    assert!(
        (db(&behind) - db(&open) - drop).abs() < 0.01,
        "the wall takes the tuning's {drop} dB: behind {behind}, open {open}"
    );
    assert_eq!(open["occlusion"].as_f64(), Some(0.0), "{open}");
}

/// v06-seq-preview-fidelity item 3: a fullscreen video section is previewed: the preview runs
/// `regia_media`'s frame publisher over its own session (never the edit world's entities, never a
/// play-in-editor world) while it plays, and the publisher's counter shows the section's picture
/// published; the viewport draws it as an overlay. Stop closes the session.
#[test]
fn fullscreen_video_section_publishes_a_frame_in_preview() {
    use regia_framework::sequencer::{Channel, VideoFit, VideoTarget};

    let (mut app, root, _open, _guid, _own) = video_preview_app("seq-preview-video-fullscreen");
    let world = app.world_mut();
    // The edit world's entities, resources aside: bevy_ecs 0.20 keeps each resource on an entity
    // of its own (`IsResource`), so the preview's own state resources would show in a bare
    // `Query<Entity>`; what must not change is the set of entities the level holds.
    let entities = |world: &mut World| -> Vec<Entity> {
        let mut query = world.query_filtered::<Entity, Without<regia_ecs::ecs::resource::IsResource>>();
        let mut all: Vec<Entity> = query.iter(world).collect();
        all.sort_unstable();
        all
    };
    let entities_before = entities(world);
    {
        let mut query = world.query::<&mut SequencePlayer>();
        for mut player in query.iter_mut(world) {
            let mut sequence = player.sequence().clone();
            if let Some(Channel::Video(sections)) = sequence.tracks.last_mut().map(|t| &mut t.keys) {
                sections[0].target = VideoTarget::Fullscreen {
                    layer: 0,
                    fit: VideoFit::Letterbox,
                };
            }
            player.replace_sequence(sequence);
        }
    }
    let out = seq_preview(world, &json!({ "on": true, "time": 1.5 })).expect("preview");
    let video = out["videos"][0].clone();
    assert_eq!(video["target"], "fullscreen", "{out}");
    assert_eq!(video["error"], Value::Null, "{out}");
    assert_eq!(video["frame"], 12, "0.5 s into the 25 fps clip: {out}");
    let published = video["publishes"].as_u64().unwrap_or(0);
    assert!(published >= 1, "the publisher published the picture: {out}");
    // A later picture is published again.
    let out = seq_preview(world, &json!({ "on": true, "time": 2.0 })).expect("later");
    let later = out["videos"][0]["publishes"].as_u64().unwrap_or(0);
    assert!(later > published, "a new picture, a new publish: {out}");
    assert_eq!(
        entities(world),
        entities_before,
        "the edit world gains no entity and loses none"
    );
    let out = seq_preview(world, &json!({ "on": false })).expect("stop");
    assert_eq!(out["on"], false);
    assert!(
        crate::seq_tracks::preview_overlay_frame(world).is_none(),
        "Stop closes the session"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// v06-seq-preview-fidelity item 1, the long-range path: a pitch-preserving section whose range of
/// sound is longer than `STRETCH_INLINE_SECS` renders off the UI thread (the async compute pool):
/// the apply that lands in it lists it `stretch_pending` and starts no voice; once the render is
/// collected (here joined, not slept on), the next apply at the same playhead starts the stretched
/// voice, and nothing renders on the apply. Counted, no wall clock.
#[test]
fn a_long_preserve_pitch_section_renders_off_the_ui_thread_then_plays() {
    use regia_framework::sequencer::{AudioSection, TimeScaleMode};

    let mut world = mixdown_world(&[("tone", sine_asset(440.0, 3.0))]);
    // 1 s of sequence at rate 2: 2 s of the sound, rendered on the pool.
    let mut section = AudioSection::new("tone", 1.0, 2.0);
    section.time_scale = 2.0;
    section.time_scale_mode = TimeScaleMode::PreservePitch;
    let mut sequence = tracks::new_sequence("intro", 3.0, 30.0).expect("new");
    sequence.tracks.push(sound_track("voice", vec![section]));
    world.spawn(SequencePlayer::new(sequence));
    let out = seq_preview(&mut world, &json!({ "on": true, "time": 1.25 })).expect("preview");
    assert_eq!(out["sounds"][0]["error"], "stretch_pending", "{out}");
    assert_eq!(out["sounds"][0]["playing"], false, "{out}");
    assert_eq!(out["voices"]["started"], 0, "{out}");
    assert_eq!(out["media"]["stretches"]["pending"], 1, "{out}");
    world
        .resource_mut::<SeqAuthoring>()
        .media
        .stretches
        .finish_pending();
    let out = seq_preview(&mut world, &json!({ "on": true })).expect("state");
    let sound = out["sounds"][0].clone();
    assert_eq!(sound["playing"], true, "the stretch is ready: {out}");
    assert_eq!(sound["stretched"], true, "{out}");
    assert_eq!(sound["error"], Value::Null, "{out}");
    assert_eq!(out["voices"]["started"], 1, "{out}");
    assert_eq!(
        out["media"]["stretches"],
        json!({ "rendered": 1, "inline": 0, "pending": 0 }),
        "rendered on the pool, not on the apply: {out}"
    );
    // The voice keeps the pitch: 440 Hz within one bin.
    let mix = render_preview_mix(&mut world, 7);
    let (hz, bin) = dominant_hz(&mix[512..512 + 4096], 48_000.0, 2_000.0);
    assert!((hz - 440.0).abs() <= bin, "{hz} Hz (bin {bin}): {out}");
}
