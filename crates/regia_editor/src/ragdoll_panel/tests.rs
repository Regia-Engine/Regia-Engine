// SPDX-License-Identifier: MPL-2.0
//! Ragdoll editor tests: open, edit, refuse, undo and save a `.ragdoll.ron`; a rig gives joint checks, the
//! humanoid generator and the gizmo; coded errors; headless drawing.

use std::path::Path;
use std::sync::Arc;

use regia_animation::Skeleton;
use regia_animation::skeleton::LocalBind;
use serde_json::{Value, json};

use super::*;
use crate::data_panels::test_ui::draw_frames;

const PROFILE: &str = r#"(
    format_version: 1,
    forward: (0.0, 0.0, -1.0),
    bones: [
        (joint: "hips", radius: 0.12, mass: 12.0, length: None, limits: None),
        (joint: "spine", radius: 0.1, mass: 9.0, length: Some(0.3),
            limits: Some((flex: Forward, bend: (-0.5, 0.8), side: (-0.3, 0.3), twist: (-0.4, 0.4)))),
    ],
)"#;

fn project(name: &str) -> std::path::PathBuf {
    let dir = regia_test_utils::unique_temp_dir(name);
    std::fs::create_dir_all(dir.join("assets/chars")).expect("dir");
    std::fs::write(dir.join("assets/chars/hero.ragdoll.ron"), PROFILE).expect("write");
    dir
}

fn world_with(dir: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.to_path_buf()),
    });
    world
}

fn bind(y: f32) -> LocalBind {
    LocalBind {
        translation: Vec3::new(0.0, y, 0.0),
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
    }
}

/// hips (root, 1 m up) → spine → head; hips → tail.
fn spawn_rig(world: &mut World) -> Entity {
    let skeleton = Skeleton::from_local_bind(
        vec!["hips".into(), "spine".into(), "head".into(), "tail".into()],
        vec![None, Some(0), Some(1), Some(0)],
        vec![bind(1.0), bind(0.25), bind(0.4), bind(-0.2)],
    )
    .expect("skeleton");
    let joints: Vec<Entity> = (0..4).map(|_| world.spawn_empty().id()).collect();
    world
        .spawn((
            SkinnedMesh {
                skeleton: Arc::new(skeleton),
                joints,
            },
            Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
        ))
        .id()
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

fn edit(world: &mut World, params: Value) -> Result<Value, RpcError> {
    call(world, "editor.ragdoll_panel_edit", params)
}

#[test]
fn open_edit_undo_and_save_round_trip() {
    let dir = project("ragdoll-panel-edit");
    let mut world = world_with(&dir);
    assert!(
        !draw_frames(&mut world, 2, ragdoll_panel).is_empty(),
        "draws with nothing open"
    );
    let v = call(
        &mut world,
        "editor.ragdoll_panel_open",
        json!({ "path": "assets/chars/hero.ragdoll.ron" }),
    )
    .expect("open");
    assert_eq!(v["profile"]["bones"][1]["limits"]["bend"][1], 0.8_f32);
    assert_eq!(v["profile"]["total_mass"], 21.0);
    edit(
        &mut world,
        json!({ "op": "set_bone", "joint": "hips", "mass": 14.0, "radius": 0.15 }),
    )
    .expect("set");
    edit(
        &mut world,
        json!({ "op": "set_bone", "joint": "spine", "limits": { "flex": "backward", "twist": [-0.2, 0.2] } }),
    )
    .expect("limits");
    let v = edit(
        &mut world,
        json!({ "op": "add_bone", "joint": "head", "radius": 0.1, "mass": 5.0 }),
    )
    .expect("add");
    assert_eq!(v["selected"], 2);
    assert_eq!(v["undo_depth"], 3);
    edit(
        &mut world,
        json!({ "op": "set_bone", "joint": "spine", "length": null }),
    )
    .expect("auto length");
    edit(&mut world, json!({ "op": "undo" })).expect("undo");
    edit(
        &mut world,
        json!({ "op": "set_forward", "forward": [0.0, 0.0, 2.0] }),
    )
    .expect("forward");
    select_and_draw(&mut world);
    let saved = call(&mut world, "editor.ragdoll_panel_save", json!({})).expect("save");
    assert_eq!(saved["written"], "assets/chars/hero.ragdoll.ron");
    let text = std::fs::read_to_string(dir.join("assets/chars/hero.ragdoll.ron")).expect("read");
    let profile = RagdollProfile::from_ron(&text).expect("valid");
    assert_eq!(profile.bones.len(), 3);
    assert_eq!((profile.bones[0].mass, profile.bones[0].radius), (14.0, 0.15));
    let spine = profile.bones[1].limits.expect("limited");
    assert_eq!(spine.flex, RagdollFlex::Backward);
    assert_eq!(spine.twist, (-0.2, 0.2));
    assert_eq!(spine.bend, (-0.5, 0.8), "fields left out stay");
    assert_eq!(profile.bones[1].length, Some(0.3), "the undone edit is not saved");
    assert_eq!(profile.forward, Vec3::Z);
    assert_eq!(
        call(&mut world, "editor.ragdoll_panel_get", json!({})).expect("get")["dirty"],
        false
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn select_and_draw(world: &mut World) {
    edit(world, json!({ "op": "select", "joint": "spine" })).expect("select");
    assert!(!draw_frames(world, 2, ragdoll_panel).is_empty());
}

#[test]
fn refused_edits_change_nothing_and_are_coded() {
    let dir = project("ragdoll-panel-refused");
    let mut world = world_with(&dir);
    call(
        &mut world,
        "editor.ragdoll_panel_open",
        json!({ "path": "assets/chars/hero.ragdoll.ron" }),
    )
    .expect("open");
    let before = call(&mut world, "editor.ragdoll_panel_get", json!({})).expect("get")["profile"].clone();
    for (params, expected) in [
        (
            json!({ "op": "set_bone", "joint": "hips", "radius": 0.0 }),
            "ragdoll_bad_bone",
        ),
        (
            json!({ "op": "set_bone", "joint": "hips", "mass": -1.0 }),
            "ragdoll_bad_bone",
        ),
        (
            json!({ "op": "set_bone", "joint": "spine", "limits": { "bend": [1.0, -1.0] } }),
            "ragdoll_bad_limits",
        ),
        (
            json!({ "op": "set_bone", "joint": "spine", "limits": { "flex": "sideways" } }),
            "ragdoll_panel_bad_value",
        ),
        (
            json!({ "op": "set_bone", "joint": "nope", "mass": 1.0 }),
            "ragdoll_panel_bone_not_found",
        ),
        (
            json!({ "op": "set_bone", "index": 9, "mass": 1.0 }),
            "ragdoll_panel_bone_not_found",
        ),
        (
            json!({ "op": "add_bone", "joint": "hips" }),
            "ragdoll_panel_duplicate_joint",
        ),
        (json!({ "op": "add_bone", "joint": "" }), "ragdoll_bad_bone"),
        (
            json!({ "op": "set_forward", "forward": [0.0, 0.0, 0.0] }),
            "ragdoll_panel_bad_value",
        ),
        (json!({ "op": "humanoid" }), "ragdoll_panel_no_rig"),
        (json!({ "op": "fly" }), "ragdoll_panel_bad_op"),
    ] {
        let e = edit(&mut world, params.clone()).expect_err("refused");
        assert_eq!(reason(&e), expected, "{params}");
    }
    let after = call(&mut world, "editor.ragdoll_panel_get", json!({})).expect("get");
    assert_eq!(after["profile"], before);
    assert_eq!(after["undo_depth"], 0);
    assert_eq!(after["error"]["code"], "ragdoll_panel_bad_op");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_rig_checks_joints_generates_a_humanoid_and_draws_the_bodies() {
    let dir = project("ragdoll-panel-rig");
    let mut world = world_with(&dir);
    let rig = spawn_rig(&mut world);
    let plain = world.spawn_empty().id();
    call(
        &mut world,
        "editor.ragdoll_panel_open",
        json!({ "path": "assets/chars/hero.ragdoll.ron" }),
    )
    .expect("open");
    let e = call(
        &mut world,
        "editor.ragdoll_panel_set_rig",
        json!({ "entity": plain.to_bits() }),
    )
    .expect_err("no mesh");
    assert_eq!(reason(&e), "ragdoll_panel_no_rig");
    assert!(ragdoll_gizmo_segments(&world).is_empty(), "no rig, no gizmo");
    edit(&mut world, json!({ "op": "add_bone", "joint": "wing" })).expect("add");
    let v = call(
        &mut world,
        "editor.ragdoll_panel_set_rig",
        json!({ "entity": rig.to_bits() }),
    )
    .expect("rig");
    let issues = v["issues"].as_array().expect("issues");
    assert!(
        issues
            .iter()
            .any(|i| i["code"] == "ragdoll_unknown_joint" && i["joint"] == "wing"),
        "{v}"
    );
    // Two bodies on the rig: 1 axis + 2 rings of 12 each; the limited spine adds 2 bend lines.
    let segments = ragdoll_gizmo_segments(&world);
    assert_eq!(segments.len(), 2 * (1 + 24) + 2);
    // The hips body starts at the hips joint, in world space (rig at x = 5).
    assert!(
        (segments[0].0 - Vec3::new(5.0, 1.0, 0.0)).length() < 1e-5,
        "{:?}",
        segments[0]
    );
    // Its length is measured to the first child (spine, 0.25 m up).
    assert!(
        (segments[0].1 - Vec3::new(5.0, 1.25, 0.0)).length() < 1e-5,
        "{:?}",
        segments[0]
    );
    let v = call(
        &mut world,
        "editor.ragdoll_panel_set_rig",
        json!({ "show_gizmo": false }),
    )
    .expect("off");
    assert_eq!(v["gizmo_segments"], 0);
    // The humanoid generator replaces the bodies with ones named after the rig's joints.
    let v = edit(&mut world, json!({ "op": "humanoid", "mass": 80.0 })).expect("humanoid");
    let joints: Vec<&str> = v["profile"]["bones"]
        .as_array()
        .expect("bones")
        .iter()
        .filter_map(|b| b["joint"].as_str())
        .collect();
    assert!(joints.contains(&"hips") && joints.contains(&"head"), "{joints:?}");
    assert!(
        joints
            .iter()
            .all(|j| ["hips", "spine", "head", "tail"].contains(j)),
        "{joints:?}"
    );
    assert!(
        v["issues"]
            .as_array()
            .is_some_and(|i| i.iter().all(|x| x["code"] != "ragdoll_unknown_joint"))
    );
    edit(&mut world, json!({ "op": "undo" })).expect("undo");
    assert!(!draw_frames(&mut world, 2, ragdoll_panel).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_profiles_paths_and_files_are_checked() {
    let dir = project("ragdoll-panel-paths");
    let mut world = world_with(&dir);
    for (path, expected) in [
        ("../x.ragdoll.ron", "ragdoll_panel_bad_path"),
        ("assets/chars/hero.ron", "ragdoll_panel_bad_path"),
        ("assets/chars/none.ragdoll.ron", "ragdoll_panel_file_not_found"),
    ] {
        let e = call(&mut world, "editor.ragdoll_panel_open", json!({ "path": path })).expect_err(path);
        assert_eq!(reason(&e), expected, "{path}");
    }
    std::fs::write(
        dir.join("assets/chars/new.ragdoll.ron"),
        "(format_version: 9, forward: (0.0, 0.0, 1.0), bones: [])",
    )
    .expect("write");
    let e = call(
        &mut world,
        "editor.ragdoll_panel_open",
        json!({ "path": "assets/chars/new.ragdoll.ron" }),
    )
    .expect_err("too new");
    assert_eq!(reason(&e), "ragdoll_format_version");
    let e = call(&mut world, "editor.ragdoll_panel_save", json!({})).expect_err("nothing open");
    assert_eq!(reason(&e), "ragdoll_panel_no_profile");
    let v = call(
        &mut world,
        "editor.ragdoll_panel_open",
        json!({ "path": "assets/chars/fresh.ragdoll.ron", "new": true }),
    )
    .expect("new");
    assert_eq!(v["issues"][0]["code"], "ragdoll_empty");
    edit(&mut world, json!({ "op": "add_bone", "joint": "hips" })).expect("add");
    call(&mut world, "editor.ragdoll_panel_save", json!({})).expect("save");
    let text = std::fs::read_to_string(dir.join("assets/chars/fresh.ragdoll.ron")).expect("read");
    assert_eq!(
        RagdollProfile::from_ron(&text).expect("valid").bones[0].joint,
        "hips"
    );
    let mut empty = World::new();
    let e = call(
        &mut empty,
        "editor.ragdoll_panel_open",
        json!({ "path": "a.ragdoll.ron" }),
    )
    .expect_err("no project");
    assert_eq!(reason(&e), "ragdoll_panel_no_project");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_method_is_registered_with_its_class() {
    let list = methods();
    assert_eq!(list.len(), 6, "open, get, edit, set_rig, preview, save");
    for (schema, _) in &list {
        assert!(schema.name.starts_with("editor.ragdoll_panel_"));
        assert_eq!(schema.mutating, schema.name != "editor.ragdoll_panel_get");
    }
}

#[test]
fn redo_brings_back_what_undo_took_and_a_new_edit_clears_it() {
    let dir = project("ragdoll-redo");
    let mut world = world_with(&dir);
    call(
        &mut world,
        "editor.ragdoll_panel_open",
        json!({ "path": "assets/chars/hero.ragdoll.ron" }),
    )
    .expect("open");
    let e = call(&mut world, "editor.ragdoll_panel_edit", json!({ "op": "redo" })).expect_err("nothing yet");
    assert_eq!(reason(&e), "ragdoll_panel_nothing_to_redo");
    call(
        &mut world,
        "editor.ragdoll_panel_edit",
        json!({ "op": "set_bone", "joint": "hips", "mass": 20.0 }),
    )
    .expect("edit");
    call(
        &mut world,
        "editor.ragdoll_panel_edit",
        json!({ "op": "set_bone", "joint": "hips", "mass": 30.0 }),
    )
    .expect("edit");
    let undone = call(&mut world, "editor.ragdoll_panel_edit", json!({ "op": "undo" })).expect("undo");
    assert_eq!(undone["profile"]["bones"][0]["mass"], 20.0);
    assert_eq!(
        (undone["undo_depth"].clone(), undone["redo_depth"].clone()),
        (json!(1), json!(1))
    );
    let redone = call(&mut world, "editor.ragdoll_panel_edit", json!({ "op": "redo" })).expect("redo");
    assert_eq!(redone["profile"]["bones"][0]["mass"], 30.0);
    assert_eq!(redone["redo_depth"], 0);
    call(&mut world, "editor.ragdoll_panel_edit", json!({ "op": "undo" })).expect("undo");
    call(&mut world, "editor.ragdoll_panel_edit", json!({ "op": "undo" })).expect("undo");
    let back = call(
        &mut world,
        "editor.ragdoll_panel_edit",
        json!({ "op": "set_bone", "joint": "hips", "radius": 0.2 }),
    )
    .expect("edit");
    assert_eq!(back["profile"]["bones"][0]["mass"], 12.0, "undone to the file");
    assert_eq!(back["redo_depth"], 0, "an edit clears the redo steps");
    let e = call(&mut world, "editor.ragdoll_panel_edit", json!({ "op": "redo" })).expect_err("cleared");
    assert_eq!(reason(&e), "ragdoll_panel_nothing_to_redo");
}

#[test]
fn a_drop_test_settles_is_deterministic_and_goes_stale_after_an_edit() {
    let dir = project("ragdoll-preview");
    let mut world = world_with(&dir);
    let rig = spawn_rig(&mut world);
    call(
        &mut world,
        "editor.ragdoll_panel_open",
        json!({ "path": "assets/chars/hero.ragdoll.ron" }),
    )
    .expect("open");
    let e = call(&mut world, "editor.ragdoll_panel_preview", json!({})).expect_err("no rig");
    assert_eq!(reason(&e), "ragdoll_panel_no_rig");
    call(
        &mut world,
        "editor.ragdoll_panel_set_rig",
        json!({ "entity": rig.to_bits() }),
    )
    .expect("rig");
    let segments_before = ragdoll_gizmo_segments(&world).len();
    let first = call(
        &mut world,
        "editor.ragdoll_panel_preview",
        json!({ "steps": 400 }),
    )
    .expect("preview");
    let p = &first["preview"];
    assert_eq!(p["stale"], false);
    assert_eq!(p["finite"], true, "{p}");
    assert_eq!(p["bodies"].as_array().map(Vec::len), Some(2), "hips and spine");
    assert!(
        p["lowest"].as_f64().unwrap_or(-1.0) > -0.05,
        "nothing sank through the floor: {p}"
    );
    assert!(p["max_speed"].as_f64().unwrap_or(0.0) > 0.5, "it fell: {p}");
    assert!(
        ragdoll_gizmo_segments(&world).len() > segments_before,
        "the pose is drawn"
    );
    let again = call(
        &mut world,
        "editor.ragdoll_panel_preview",
        json!({ "steps": 400 }),
    )
    .expect("preview");
    assert_eq!(again["preview"]["digest"], p["digest"], "same profile, same fall");

    call(
        &mut world,
        "editor.ragdoll_panel_edit",
        json!({ "op": "set_bone", "joint": "hips", "mass": 40.0 }),
    )
    .expect("edit");
    let state = call(&mut world, "editor.ragdoll_panel_get", json!({})).expect("get");
    assert_eq!(state["preview"]["stale"], true);
    assert_eq!(
        ragdoll_gizmo_segments(&world).len(),
        segments_before,
        "a stale pose is not drawn"
    );

    for bad in [
        json!({ "steps": 0 }),
        json!({ "steps": 5000 }),
        json!({ "drop_height": -1.0 }),
        json!({ "drop_height": 50.0 }),
    ] {
        let e = call(&mut world, "editor.ragdoll_panel_preview", bad.clone()).expect_err("out of range");
        assert_eq!(reason(&e), "ragdoll_preview_bad_options", "{bad}");
    }
    for bad in [json!({ "steps": "many" }), json!({ "push": [1, 2] })] {
        let e = call(&mut world, "editor.ragdoll_panel_preview", bad.clone()).expect_err("bad value");
        assert_eq!(reason(&e), "ragdoll_panel_bad_value", "{bad}");
    }
}

#[test]
fn a_profile_with_no_bone_on_the_rig_has_no_drop_test() {
    let dir = project("ragdoll-preview-none");
    let mut world = world_with(&dir);
    let rig = spawn_rig(&mut world);
    call(
        &mut world,
        "editor.ragdoll_panel_open",
        json!({ "path": "assets/chars/new.ragdoll.ron", "new": true }),
    )
    .expect("new");
    call(
        &mut world,
        "editor.ragdoll_panel_set_rig",
        json!({ "entity": rig.to_bits() }),
    )
    .expect("rig");
    let e = call(&mut world, "editor.ragdoll_panel_preview", json!({})).expect_err("no bodies");
    assert_eq!(reason(&e), "ragdoll_preview_no_bodies");
}

#[test]
fn the_drop_test_and_redo_buttons_draw_headless() {
    let dir = project("ragdoll-preview-draw");
    let mut world = world_with(&dir);
    let rig = spawn_rig(&mut world);
    call(
        &mut world,
        "editor.ragdoll_panel_open",
        json!({ "path": "assets/chars/hero.ragdoll.ron" }),
    )
    .expect("open");
    call(
        &mut world,
        "editor.ragdoll_panel_set_rig",
        json!({ "entity": rig.to_bits() }),
    )
    .expect("rig");
    call(&mut world, "editor.ragdoll_panel_preview", json!({ "steps": 60 })).expect("preview");
    draw_frames(&mut world, 2, ragdoll_panel);
    assert!(world.resource::<RagdollPanel>().preview.is_some());
}

/// Bodies switch between capsule, sphere and box (ADR-2133 § 6): the gizmo draws each shape, wrong values
/// are refused with codes, undo brings the capsule back, save writes v2 shapes, and a drop test of a box and
/// sphere ragdoll settles the same way twice.
#[test]
fn bodies_switch_shapes_draw_them_save_them_and_drop_test_them() {
    let dir = project("ragdoll-panel-shapes");
    let mut world = world_with(&dir);
    let rig = spawn_rig(&mut world);
    call(
        &mut world,
        "editor.ragdoll_panel_open",
        json!({ "path": "assets/chars/hero.ragdoll.ron" }),
    )
    .expect("open");
    call(
        &mut world,
        "editor.ragdoll_panel_set_rig",
        json!({ "entity": rig.to_bits() }),
    )
    .expect("rig");
    let capsules = ragdoll_gizmo_segments(&world).len();
    assert_eq!(capsules, 2 * (1 + 24) + 2);

    let v = edit(
        &mut world,
        json!({ "op": "set_bone", "joint": "hips", "shape": "box" }),
    )
    .expect("box");
    // A new box starts at the bone's diameter on both sides.
    assert_eq!(
        v["profile"]["bones"][0]["shape"],
        json!({ "kind": "box", "width": 0.24_f32, "depth": 0.24_f32 })
    );
    edit(
        &mut world,
        json!({ "op": "set_bone", "joint": "hips", "width": 0.36, "depth": 0.2 }),
    )
    .expect("box size");
    // The hips box replaces two 12-segment rings with 12 edges.
    assert_eq!(ragdoll_gizmo_segments(&world).len(), capsules - 24 + 12);
    let v = edit(
        &mut world,
        json!({ "op": "add_bone", "joint": "head", "shape": "sphere", "radius": 0.1, "mass": 5.0 }),
    )
    .expect("sphere");
    assert_eq!(v["profile"]["bones"][2]["shape"]["kind"], "sphere");
    // A sphere: the axis and three rings.
    assert_eq!(ragdoll_gizmo_segments(&world).len(), capsules - 24 + 12 + 1 + 36);

    for (params, expected) in [
        (
            json!({ "op": "set_bone", "joint": "hips", "width": 0.0 }),
            "ragdoll_bad_bone",
        ),
        (
            json!({ "op": "set_bone", "joint": "spine", "width": 0.3 }),
            "ragdoll_panel_bad_value",
        ),
        (
            json!({ "op": "set_bone", "joint": "spine", "shape": "cone" }),
            "ragdoll_panel_bad_value",
        ),
        (
            json!({ "op": "set_bone", "joint": "spine", "shape": 3 }),
            "ragdoll_panel_bad_value",
        ),
    ] {
        let before = call(&mut world, "editor.ragdoll_panel_get", json!({})).expect("get")["profile"].clone();
        let e = edit(&mut world, params.clone()).expect_err("refused");
        assert_eq!(reason(&e), expected, "{params}");
        let after = call(&mut world, "editor.ragdoll_panel_get", json!({})).expect("get")["profile"].clone();
        assert_eq!(before, after, "a refused edit changes nothing: {params}");
    }

    // The box and sphere ragdoll drop test is deterministic.
    let first = call(
        &mut world,
        "editor.ragdoll_panel_preview",
        json!({ "steps": 200 }),
    )
    .expect("preview");
    let again = call(
        &mut world,
        "editor.ragdoll_panel_preview",
        json!({ "steps": 200 }),
    )
    .expect("preview");
    assert_eq!(first["preview"]["digest"], again["preview"]["digest"]);
    assert_eq!(first["preview"]["finite"], true);
    assert_eq!(first["preview"]["bodies"].as_array().map(Vec::len), Some(3));

    select_and_draw(&mut world);
    edit(&mut world, json!({ "op": "select", "joint": "hips" })).expect("select");
    assert!(
        !draw_frames(&mut world, 2, ragdoll_panel).is_empty(),
        "the box size row draws"
    );

    let saved = call(&mut world, "editor.ragdoll_panel_save", json!({})).expect("save");
    assert_eq!(saved["written"], "assets/chars/hero.ragdoll.ron");
    let text = std::fs::read_to_string(dir.join("assets/chars/hero.ragdoll.ron")).expect("read");
    let current = format!(
        "format_version: {}",
        regia_framework::ragdoll::file::RAGDOLL_FORMAT_VERSION
    );
    assert!(text.contains(&current), "{text}");
    let profile = RagdollProfile::from_ron(&text).expect("valid");
    assert_eq!(
        profile.bones[0].shape,
        RagdollShape::Box {
            width: 0.36,
            depth: 0.2
        }
    );
    assert_eq!(profile.bones[1].shape, RagdollShape::Capsule);
    assert_eq!(profile.bones[2].shape, RagdollShape::Sphere);

    // Undo the sphere, both box edits: the hips are a capsule again.
    for _ in 0..3 {
        edit(&mut world, json!({ "op": "undo" })).expect("undo");
    }
    assert_eq!(ragdoll_gizmo_segments(&world).len(), capsules);
    let _ = std::fs::remove_dir_all(&dir);
}

const HERO: &str = "assets/chars/hero.ragdoll.ron";

/// An editor world: the project and the shared undo history.
fn hosted_world(dir: &Path) -> World {
    let mut world = world_with(dir);
    world.insert_resource(crate::command::UndoStack::new());
    world
}

fn hips_mass(world: &World) -> f32 {
    world
        .resource::<RagdollPanel>()
        .profile
        .as_ref()
        .expect("profile")
        .bones[0]
        .mass
}

/// Global undo (P-322): in an editor world a ragdoll edit is an entry of the shared history, which
/// the editor's Undo and Redo (and the panel's own) step; a refused edit adds none.
#[test]
fn an_edit_in_ragdoll_panel_undoes_from_the_global_stack() {
    let dir = project("ragdoll-panel-global-undo");
    let mut world = hosted_world(&dir);
    call(&mut world, "editor.ragdoll_panel_open", json!({ "path": HERO })).expect("open");
    edit(
        &mut world,
        json!({ "op": "set_bone", "joint": "hips", "mass": 14.0 }),
    )
    .expect("set");
    let v = edit(
        &mut world,
        json!({ "op": "add_bone", "joint": "head", "radius": 0.1 }),
    )
    .expect("add");
    assert_eq!(v["undo_depth"], 2, "{v}");
    assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 2);
    edit(
        &mut world,
        json!({ "op": "set_bone", "joint": "hips", "mass": -1.0 }),
    )
    .expect_err("refused");
    assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 2);
    // The editor's Undo (Edit ▸ Undo, Ctrl+Z, editor.undo).
    assert!(crate::command::undo_world(&mut world));
    assert_eq!(
        world
            .resource::<RagdollPanel>()
            .profile
            .as_ref()
            .expect("profile")
            .bones
            .len(),
        2
    );
    // The panel's own Undo steps the same history.
    let v = edit(&mut world, json!({ "op": "undo" })).expect("panel undo");
    assert_eq!(hips_mass(&world), 12.0);
    assert_eq!(
        (v["undo_depth"].clone(), v["redo_depth"].clone()),
        (json!(0), json!(2)),
        "{v}"
    );
    assert_eq!(
        reason(&edit(&mut world, json!({ "op": "undo" })).expect_err("nothing left")),
        "ragdoll_panel_nothing_to_undo"
    );
    edit(&mut world, json!({ "op": "redo" })).expect("panel redo");
    assert_eq!(hips_mass(&world), 14.0);
    assert!(crate::command::redo_world(&mut world));
    assert_eq!(
        world
            .resource::<RagdollPanel>()
            .profile
            .as_ref()
            .expect("profile")
            .bones
            .len(),
        3
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Global undo (P-322): the Undo History panel lists a ragdoll edit with its file and joint;
/// undoing it after another profile was opened reopens the edited one.
#[test]
fn an_edit_in_ragdoll_panel_appears_in_undo_history() {
    let dir = project("ragdoll-panel-global-history");
    std::fs::write(dir.join("assets/chars/other.ragdoll.ron"), PROFILE).expect("other");
    let mut world = hosted_world(&dir);
    call(&mut world, "editor.ragdoll_panel_open", json!({ "path": HERO })).expect("open");
    edit(
        &mut world,
        json!({ "op": "set_bone", "joint": "hips", "mass": 14.0 }),
    )
    .expect("set");
    let history = world.resource::<crate::command::UndoStack>().history();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].documents, vec![HERO.to_owned()], "{history:?}");
    assert_eq!(history[0].touched[0].property.as_deref(), Some("bones/hips"));
    assert!(
        history[0].description.contains(HERO),
        "{}",
        history[0].description
    );
    assert!(
        !world.resource::<crate::command::UndoStack>().is_dirty(),
        "a ragdoll edit does not dirty the level"
    );
    call(&mut world, "editor.ragdoll_panel_save", json!({})).expect("save");
    call(
        &mut world,
        "editor.ragdoll_panel_open",
        json!({ "path": "assets/chars/other.ragdoll.ron" }),
    )
    .expect("open other");
    assert!(crate::command::undo_world(&mut world));
    let v = call(&mut world, "editor.ragdoll_panel_get", json!({})).expect("get");
    assert_eq!(v["path"], HERO, "undo reopens the edited profile");
    assert_eq!(hips_mass(&world), 12.0);
    assert_eq!(v["dirty"], true);
    let _ = std::fs::remove_dir_all(&dir);
}
