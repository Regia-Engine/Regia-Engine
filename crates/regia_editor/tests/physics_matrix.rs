// SPDX-License-Identifier: MPL-2.0
//! The physics layer matrix editor (`docs/specs/19-editor.md` § 2, ADR-7233) end to end, headless:
//! the agent methods edit `config/physics.ron` (all or nothing, undoable), the Project Settings
//! section draws the matrix, and the runtime honors the file the editor wrote: a box on a layer the
//! matrix lets collide with the ground rests on it, and falls through once the pair is switched off.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::{
    Collider, ColliderShape, CollisionLayer, LayerFile, PhysicsLayers, PhysicsPlugin, RigidBody,
};
use regia_scene::Transform;
use serde_json::{Value, json};

fn temp_project(label: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(&format!("physics-matrix-{label}"));
    std::fs::create_dir_all(dir.join("assets/levels")).expect("assets");
    std::fs::write(
        dir.join("Regia.toml"),
        "[project]\nname = \"Matrix\"\nid = \"com.example.matrix\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    dir
}

fn edit_app(root: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<regia_editor::command::UndoStack>()
        .init_resource::<regia_editor::selection::Selection>()
        .init_resource::<regia_editor::viewport::EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>();
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
    app.update();
    app
}

/// Calls an editor agent method through the installed extension; the error code on failure.
fn call(world: &mut World, method: &str, params: Value) -> Result<Value, String> {
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params).map_err(|e| {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .map_or_else(|| e.message.clone(), str::to_owned)
    })
}

/// Where a 1 m box dropped from y = 2 onto a plate (top face y = 0) is after 2 s, with the
/// layers loaded the way a game loads them (`LayerFile::load` + `PhysicsLayers::from_file`).
fn drop_box_with_project_layers(root: &Path) -> f32 {
    let file = LayerFile::load(&root.join("config/physics.ron")).expect("the runtime reads the file");
    let layers = PhysicsLayers::from_file(&file).expect("the runtime builds the layer table");
    let world_layer = layers.index_of("world").expect("world layer");
    let props_layer = layers.index_of("props").expect("props layer");
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app.insert_resource(layers);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(5.0, 0.5, 5.0),
            },
            layer: CollisionLayer::new(world_layer),
            ..Collider::default()
        },
    ));
    let falling = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.5),
                },
                layer: CollisionLayer::new(props_layer),
                ..Collider::default()
            },
        ))
        .id();
    for _ in 0..120 {
        app.update();
    }
    app.world()
        .get::<Transform>(falling)
        .map_or(f32::NAN, |t| t.translation.y)
}

#[test]
fn the_matrix_editor_writes_the_project_setting_and_the_runtime_honors_it() {
    let root = temp_project("honor");
    let mut app = edit_app(&root);
    let file = root.join("config/physics.ron");

    // No file yet: the engine default (one self-colliding layer) is shown.
    let got = call(app.world_mut(), "editor.physics_layers_get", json!({})).expect("get");
    assert_eq!(got["exists"], json!(false));
    assert_eq!(got["layers"], json!(["default"]));
    assert_eq!(got["collides"], json!([["default", "default"]]));

    let out = call(
        app.world_mut(),
        "editor.physics_layers_edit",
        json!({ "edits": [
            { "op": "rename_layer", "from": "default", "to": "world" },
            { "op": "add_layer", "name": "props" },
            { "op": "set_collides", "a": "props", "b": "world", "on": true },
            { "op": "set_query", "from": "props", "seen": "world", "on": true },
        ] }),
    )
    .expect("edit");
    assert_eq!(out["exists"], json!(true));
    assert_eq!(out["layers"], json!(["world", "props"]));
    // Pairs come back canonical: lower layer index first, sorted.
    assert_eq!(out["collides"], json!([["world", "world"], ["world", "props"]]));
    assert_eq!(out["queries"], json!([["world", "world"], ["props", "world"]]));
    assert!(file.is_file(), "config/physics.ron written");

    let resting = drop_box_with_project_layers(&root);
    assert!(
        (resting - 0.5).abs() < 0.05,
        "props collide with world: the box rests on the plate (y = {resting})"
    );

    call(
        app.world_mut(),
        "editor.physics_layers_edit",
        json!({ "edits": [{ "op": "set_collides", "a": "world", "b": "props", "on": false }] }),
    )
    .expect("switch the pair off");
    let fallen = drop_box_with_project_layers(&root);
    assert!(
        fallen < -2.0,
        "the pair is off: the box falls through the plate (y = {fallen})"
    );

    // Undo puts the pair back (the file is the undo entry's state).
    call(app.world_mut(), "editor.undo", json!({})).expect("undo");
    let got = call(app.world_mut(), "editor.physics_layers_get", json!({})).expect("get");
    assert_eq!(got["collides"], json!([["world", "world"], ["world", "props"]]));
    let again = drop_box_with_project_layers(&root);
    assert!(
        (again - 0.5).abs() < 0.05,
        "after undo the box rests again (y = {again})"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn refused_matrix_edits_write_nothing() {
    let root = temp_project("refuse");
    let mut app = edit_app(&root);
    let file = root.join("config/physics.ron");
    call(
        app.world_mut(),
        "editor.physics_layers_edit",
        json!({ "edits": [
            { "op": "add_layer", "name": "player" },
            { "op": "add_layer", "name": "trigger" },
        ] }),
    )
    .expect("seed");
    let text = std::fs::read_to_string(&file).expect("written");
    for (edits, code) in [
        (
            json!([{ "op": "add_layer", "name": "player" }]),
            "duplicate_layer",
        ),
        (json!([{ "op": "add_layer", "name": " " }]), "bad_layer_name"),
        (json!([{ "op": "add_layer", "name": "a\"b" }]), "bad_layer_name"),
        (
            json!([{ "op": "set_collides", "a": "player", "b": "ghost", "on": true }]),
            "unknown_layer",
        ),
        (
            json!([{ "op": "set_query", "from": "ghost", "seen": "player", "on": true }]),
            "unknown_layer",
        ),
        (
            json!([{ "op": "rename_layer", "from": "player", "to": "trigger" }]),
            "duplicate_layer",
        ),
        // Colliders store layer indices: only the last layer may be removed.
        (
            json!([{ "op": "remove_layer", "name": "player" }]),
            "layer_not_last",
        ),
        (json!([{ "op": "explode" }]), "bad_physics_edit"),
        (
            json!([{ "op": "set_collides", "a": "player", "b": "trigger" }]),
            "bad_physics_edit",
        ),
        // All or nothing: the valid first edit is not kept.
        (
            json!([{ "op": "add_layer", "name": "enemy" }, { "op": "add_layer", "name": "enemy" }]),
            "duplicate_layer",
        ),
    ] {
        assert_eq!(
            call(
                app.world_mut(),
                "editor.physics_layers_edit",
                json!({ "edits": edits.clone() })
            )
            .unwrap_err(),
            code,
            "{edits}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).expect("file"),
            text,
            "{edits} wrote"
        );
    }
    assert_eq!(
        call(app.world_mut(), "editor.physics_layers_edit", json!({})).unwrap_err(),
        "bad_physics_edit"
    );
    // 32 layers fit; a 33rd is refused.
    let fill: Vec<Value> = (3..32)
        .map(|i| json!({ "op": "add_layer", "name": format!("l{i}") }))
        .collect();
    call(
        app.world_mut(),
        "editor.physics_layers_edit",
        json!({ "edits": fill }),
    )
    .expect("32 layers");
    assert_eq!(
        call(
            app.world_mut(),
            "editor.physics_layers_edit",
            json!({ "edits": [{ "op": "add_layer", "name": "l32" }] })
        )
        .unwrap_err(),
        "too_many_layers"
    );
    // Removing the last layer drops its pairs.
    let out = call(
        app.world_mut(),
        "editor.physics_layers_edit",
        json!({ "edits": [
            { "op": "set_collides", "a": "l31", "b": "player", "on": true },
            { "op": "remove_layer", "name": "l31" },
        ] }),
    )
    .expect("remove last");
    assert_eq!(out["layers"].as_array().map(Vec::len), Some(31));
    assert!(
        !out["collides"].to_string().contains("l31"),
        "the removed layer's pairs are gone: {}",
        out["collides"]
    );
    // A file the runtime cannot read is reported, never overwritten.
    std::fs::write(
        &file,
        "(layers: [\"a\"], collides: [(\"a\", \"ghost\")], queries: [])",
    )
    .expect("bad file");
    assert_eq!(
        call(app.world_mut(), "editor.physics_layers_get", json!({})).unwrap_err(),
        "physics_config_invalid"
    );
    assert_eq!(
        call(
            app.world_mut(),
            "editor.physics_layers_edit",
            json!({ "edits": [{ "op": "add_layer", "name": "x" }] })
        )
        .unwrap_err(),
        "physics_config_invalid"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// The Project Settings section draws without a GPU and offers the matrix.
#[test]
fn the_matrix_section_draws_the_layers() {
    let root = temp_project("draw");
    let mut app = edit_app(&root);
    call(
        app.world_mut(),
        "editor.physics_layers_edit",
        json!({ "edits": [{ "op": "add_layer", "name": "props" }] }),
    )
    .expect("seed");
    let ctx = egui::Context::default();
    let mut undo = Vec::new();
    let mut seen = false;
    for frame in 0..2 {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            time: Some(f64::from(frame) / 60.0),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            regia_editor::physics_matrix::matrix_section(ui, &root, &mut undo);
        });
        output.textures_delta.clear();
        seen |= output
            .shapes
            .iter()
            .any(|shape| format!("{:?}", shape.shape).contains("props"));
    }
    assert!(seen, "the matrix names the props layer");
    assert!(undo.is_empty(), "drawing alone writes nothing");
    let _ = std::fs::remove_dir_all(root);
}
