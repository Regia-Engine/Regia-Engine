// SPDX-License-Identifier: MPL-2.0
//! Montage timeline tests: lanes from `anim.montage_describe`, edits through `anim.montage_edit` (followers,
//! undo), the section-chain and playhead preview, unknown clip lengths, coded errors, headless drawing.

use std::path::Path;

use serde_json::{Value, json};

use super::*;
use crate::data_panels::test_ui::draw_frames;

const COMBO: &str = r#"(format_version: 2, name: "combo", tracks: [(slot: "full", segments: [
    (clip: "a", end: Some(1.0)), (clip: "b", end: Some(1.0))])],
    sections: [(name: "windup", start: 0.0), (name: "swing", start: 1.0,
        follow: Some((track: 0, segment: 1, mode: ShiftWithStart)))],
    notifies: [(name: "hit", time: 1.25)],
    warps: [(target: "enemy", start: 0.2, end: 0.8)])"#;

const OPEN_END: &str =
    r#"(format_version: 2, name: "wave", tracks: [(slot: "full", segments: [(clip: "w")])])"#;

fn world_at(root: &Path) -> World {
    std::fs::create_dir_all(root.join("anim")).expect("dir");
    std::fs::write(root.join("anim/combo.montage.ron"), COMBO).expect("write");
    std::fs::write(root.join("anim/wave.montage.ron"), OPEN_END).expect("write");
    let mut world = World::new();
    let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: regia_asset_pipeline::host_platform().to_string(),
        content_dir: None,
    })
    .expect("opens");
    world.insert_resource(server);
    world
}

fn call(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
    let handler = methods()
        .into_iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| h)
        .expect("registered");
    handler(world, &params)
}

fn reason(e: &RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn chain(state: &Value) -> Vec<String> {
    state["preview"]["chain"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["section"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn open_lays_out_the_lanes_and_draws_headless() {
    let root = regia_test_utils::unique_temp_dir("montage-panel-open");
    let mut world = world_at(&root);
    assert!(
        !draw_frames(&mut world, 2, montage_panel).is_empty(),
        "draws with nothing open"
    );
    let v = call(
        &mut world,
        "editor.montage_panel_open",
        json!({ "asset": "anim/combo.montage.ron" }),
    )
    .expect("open");
    assert_eq!(v["montage"]["name"], "combo");
    assert_eq!(v["layout"][1]["at"], 1.0);
    assert_eq!(v["layout"][1]["known"], true);
    assert_eq!(v["preview"]["length"], 2.0);
    assert_eq!(chain(&v), ["windup", "swing"]);
    assert_eq!(v["preview"]["loops"], false);
    call(
        &mut world,
        "editor.montage_panel_select",
        json!({ "kind": "section", "name": "swing" }),
    )
    .expect("select");
    assert!(!draw_frames(&mut world, 2, montage_panel).is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn edits_go_through_montage_edit_with_followers_and_undo() {
    let root = regia_test_utils::unique_temp_dir("montage-panel-edit");
    let mut world = world_at(&root);
    call(
        &mut world,
        "editor.montage_panel_open",
        json!({ "asset": "anim/combo.montage.ron" }),
    )
    .expect("open");
    // Trim segment 0: `swing` follows segment 1 to 0.5 and the lanes move with it.
    let v = call(
        &mut world,
        "editor.montage_panel_edit",
        json!({ "edit": { "op": "move_segment", "track": 0, "segment": 0, "end": 0.5 } }),
    )
    .expect("edit");
    assert_eq!(v["report"]["moved"][0]["name"], "swing");
    assert_eq!(v["layout"][1]["at"], 0.5);
    assert_eq!(v["preview"]["chain"][1]["start"], 0.5);
    assert_eq!(v["last_edit"]["undo_depth"], 1);
    // A dropped clip joins the track at its end.
    let v = call(
        &mut world,
        "editor.montage_panel_edit",
        json!({ "edit": { "op": "add_segment", "slot": "full", "clip": clip_name_of("assets/anim/kick.clip.ron"), "end": 0.75 } }),
    )
    .expect("add");
    assert_eq!(v["layout"][2]["clip"], "kick");
    assert_eq!(v["layout"][2]["at"], 1.5);
    // A refused edit writes nothing.
    let before = std::fs::read_to_string(root.join("anim/combo.montage.ron")).expect("read");
    let e = call(
        &mut world,
        "editor.montage_panel_edit",
        json!({ "edit": { "op": "remove_segment", "track": 4, "segment": 0 } }),
    )
    .expect_err("no track 4");
    assert!(reason(&e).starts_with("montage_"), "{}", reason(&e));
    assert_eq!(
        std::fs::read_to_string(root.join("anim/combo.montage.ron")).expect("read"),
        before
    );
    // Two undos bring the file back.
    for _ in 0..2 {
        call(
            &mut world,
            "editor.montage_panel_edit",
            json!({ "edit": { "op": "undo" } }),
        )
        .expect("undo");
    }
    assert_eq!(
        std::fs::read_to_string(root.join("anim/combo.montage.ron")).expect("read"),
        COMBO
    );
    let v = call(&mut world, "editor.montage_panel_get", json!({})).expect("get");
    assert_eq!(v["layout"][1]["at"], 1.0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_preview_follows_section_links_and_reports_loops() {
    let root = regia_test_utils::unique_temp_dir("montage-panel-chain");
    let mut world = world_at(&root);
    call(
        &mut world,
        "editor.montage_panel_open",
        json!({ "asset": "anim/combo.montage.ron" }),
    )
    .expect("open");
    call(
        &mut world,
        "editor.montage_panel_edit",
        json!({ "edit": { "op": "link_sections", "from": "swing", "to": "windup" } }),
    )
    .expect("link");
    let v = call(
        &mut world,
        "editor.montage_panel_preview",
        json!({ "section": "swing", "playhead": 1.5 }),
    )
    .expect("preview");
    assert_eq!(chain(&v), ["swing", "windup"]);
    assert_eq!(v["preview"]["loops"], true);
    assert_eq!(v["preview"]["active"][0]["clip"], "b");
    assert_eq!(v["preview"]["active"][0]["clip_time"], 0.5);
    let v = call(
        &mut world,
        "editor.montage_panel_preview",
        json!({ "whole": true, "playhead": 99.0 }),
    )
    .expect("whole");
    assert_eq!(chain(&v), ["windup", "swing"]);
    assert_eq!(v["preview"]["playhead"], 2.0, "clamped to the timeline");
    call(
        &mut world,
        "editor.montage_panel_edit",
        json!({ "edit": { "op": "link_sections", "from": "windup", "stop": true } }),
    )
    .expect("stop");
    let v = call(
        &mut world,
        "editor.montage_panel_preview",
        json!({ "whole": false, "section": null }),
    )
    .expect("chain");
    assert_eq!(chain(&v), ["windup"]);
    assert_eq!(v["preview"]["loops"], false);
    let e = call(
        &mut world,
        "editor.montage_panel_preview",
        json!({ "section": "nope" }),
    )
    .expect_err("no section");
    assert_eq!(reason(&e), "montage_panel_section_not_found");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unknown_clip_lengths_are_placed_nominally_and_reported() {
    let root = regia_test_utils::unique_temp_dir("montage-panel-unknown");
    let mut world = world_at(&root);
    let v = call(
        &mut world,
        "editor.montage_panel_open",
        json!({ "asset": "anim/wave.montage.ron" }),
    )
    .expect("open");
    assert_eq!(v["layout"][0]["known"], false);
    assert_eq!(v["layout"][0]["length"], UNKNOWN_LENGTH);
    assert!(
        v["montage"]["issues"]
            .as_array()
            .is_some_and(|i| i.iter().any(|x| x["code"] == "montage_clip_length_unknown"))
    );
    assert_eq!(chain(&v), ["default"]);
    assert!(!draw_frames(&mut world, 2, montage_panel).is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn errors_are_coded_and_paths_confined() {
    let root = regia_test_utils::unique_temp_dir("montage-panel-errors");
    let mut world = world_at(&root);
    let e = call(
        &mut world,
        "editor.montage_panel_edit",
        json!({ "edit": { "op": "undo" } }),
    )
    .expect_err("nothing open");
    assert_eq!(reason(&e), "montage_panel_no_asset");
    let e = call(&mut world, "editor.montage_panel_preview", json!({})).expect_err("nothing open");
    assert_eq!(reason(&e), "montage_panel_no_asset");
    for asset in ["../x.montage.ron", "/abs.montage.ron", "anim/combo.txt"] {
        let e = call(&mut world, "editor.montage_panel_open", json!({ "asset": asset })).expect_err(asset);
        assert_eq!(reason(&e), "bad_path", "{asset}");
    }
    call(
        &mut world,
        "editor.montage_panel_open",
        json!({ "asset": "anim/combo.montage.ron" }),
    )
    .expect("open");
    let e = call(&mut world, "editor.montage_panel_edit", json!({ "edit": 3 })).expect_err("not an object");
    assert_eq!(reason(&e), "montage_bad_edit");
    let e = call(
        &mut world,
        "editor.montage_panel_edit",
        json!({ "edit": { "op": "explode" } }),
    )
    .expect_err("bad op");
    assert_eq!(reason(&e), "montage_bad_edit");
    let v = call(&mut world, "editor.montage_panel_get", json!({})).expect("get");
    assert_eq!(
        v["error"]["code"], "montage_bad_edit",
        "the last failure stays on the error line"
    );
    let e = call(
        &mut world,
        "editor.montage_panel_select",
        json!({ "kind": "bone" }),
    )
    .expect_err("kind");
    assert_eq!(reason(&e), "montage_panel_bad_value");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn chain_and_clip_time_are_pure_functions_of_the_description() {
    let described = json!({
        "length": null,
        "tracks": [{ "slot": "full", "segments": [
            { "clip": "a", "start": 0.5, "end": 1.0, "rate": 2.0, "loops": 2, "length": 0.5 },
            { "clip": "b", "start": 0.0, "end": null, "rate": 1.0, "loops": 1, "length": null },
        ] }],
        "sections": [],
    });
    let placed = layout(&described);
    assert_eq!(placed[1].at, 0.5);
    assert!(!placed[1].known);
    assert_eq!(timeline_length(&described), 0.5 + UNKNOWN_LENGTH);
    // Segment a: 0.5 s on the timeline plays clip time 0.5..1.0 twice at rate 2.
    let at = |t: f32| active_at(&described, t)[0]["clip_time"].as_f64().expect("time") as f32;
    assert!((at(0.0) - 0.5).abs() < 1e-6);
    assert!((at(0.125) - 0.75).abs() < 1e-6);
    assert!((at(0.25) - 0.5).abs() < 1e-6, "the second loop starts over");
    assert_eq!(active_at(&described, 0.75)[0]["clip"], "b");
    assert!(active_at(&described, 5.0).is_empty());
    assert_eq!(clip_name_of("assets/anim/Swing.Heavy.clip.ron"), "Swing");
    assert_eq!(clip_name_of("kick"), "kick");
}

#[test]
fn every_method_is_registered_with_its_class() {
    let list = methods();
    // open, get, edit, preview, select, slot_edit (w11 forms-undo-autosave).
    assert_eq!(list.len(), 6);
    for (schema, _) in &list {
        assert!(schema.name.starts_with("editor.montage_panel_"));
        assert_eq!(schema.mutating, schema.name != "editor.montage_panel_get");
    }
}

/// Spec 34 M10: the pose preview samples the main track at the playhead on a picked rig.
#[test]
fn the_pose_preview_samples_the_main_track_on_a_rig() {
    use regia_animation::clip::{JointTrack, TrackCurve};
    use regia_animation::skeleton::LocalBind;
    use regia_animation::{AnimationClip, ClipLibrary, MontagePlayer, Skeleton, SkinnedMesh};
    use regia_core::math::{Quat, Vec3};

    let root = regia_test_utils::unique_temp_dir("montage-panel-pose");
    let mut world = world_at(&root);
    let bind = |y: f32| LocalBind {
        translation: Vec3::new(0.0, y, 0.0),
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
    };
    let skeleton = Skeleton::from_local_bind(
        vec!["root".into(), "arm".into()],
        vec![None, Some(0)],
        vec![bind(0.0), bind(0.25)],
    )
    .expect("skeleton");
    // Clip `a` slides the arm from x = 0 to x = 1 over its second; clip `b` holds the bind pose.
    let mut a = AnimationClip::empty("a", 1.0);
    a.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![Vec3::new(0.0, 0.25, 0.0), Vec3::new(1.0, 0.25, 0.0)],
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    let mut clips = ClipLibrary::new();
    clips.insert(a);
    clips.insert(AnimationClip::empty("b", 1.0));
    let joints: Vec<Entity> = (0..2).map(|_| world.spawn_empty().id()).collect();
    let rig = world
        .spawn((
            SkinnedMesh {
                skeleton: std::sync::Arc::new(skeleton),
                joints,
            },
            MontagePlayer::new(Vec::new(), clips),
            regia_scene::prelude::Transform::from_translation(Vec3::new(2.0, 0.0, 0.0)),
        ))
        .id();
    let plain = world.spawn_empty().id();
    call(
        &mut world,
        "editor.montage_panel_open",
        json!({ "asset": "anim/combo.montage.ron" }),
    )
    .expect("open");
    assert!(montage_pose_segments(&world).is_empty(), "no rig yet");
    let e = call(
        &mut world,
        "editor.montage_panel_preview",
        json!({ "rig": plain.to_bits() }),
    )
    .expect_err("no mesh");
    assert_eq!(reason(&e), "montage_panel_no_rig");
    let v = call(
        &mut world,
        "editor.montage_panel_preview",
        json!({ "rig": rig.to_bits(), "playhead": 0.5 }),
    )
    .expect("preview");
    let pose = &v["preview"]["pose"];
    assert_eq!(pose["sampled"], true, "{v}");
    assert_eq!(pose["joints"], 2);
    let arm = &pose["positions"][1];
    let close = |value: &Value, want: f32| (value.as_f64().expect("number") as f32 - want).abs() < 1e-5;
    assert!(
        close(&arm[0], 2.5) && close(&arm[1], 0.25),
        "the arm is halfway, on the rig at x = 2: {arm}"
    );
    assert_eq!(montage_pose_segments(&world).len(), 1, "one bone line");
    // In clip `b` (playhead 1.5) the arm is back at its bind position.
    let v = call(
        &mut world,
        "editor.montage_panel_preview",
        json!({ "playhead": 1.5 }),
    )
    .expect("preview");
    assert!(close(&v["preview"]["pose"]["positions"][1][0], 2.0));
    // A montage naming a clip the rig lacks says so.
    call(
        &mut world,
        "editor.montage_panel_edit",
        json!({ "edit": { "op": "add_segment", "slot": "full", "clip": "missing", "end": 0.5 } }),
    )
    .expect("add");
    let v = call(&mut world, "editor.montage_panel_get", json!({})).expect("get");
    assert_eq!(
        v["preview"]["pose"]["error"]["code"], "montage_panel_missing_clip",
        "{v}"
    );
    assert!(montage_pose_segments(&world).is_empty());
    let v = call(&mut world, "editor.montage_panel_preview", json!({ "rig": null })).expect("clear");
    assert!(v["preview"]["pose"].is_null());
    let _ = std::fs::remove_dir_all(&root);
}
