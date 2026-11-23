// SPDX-License-Identifier: MPL-2.0
//! The Inspector pickers driven through egui input and the agent dispatcher (ADR-7786): asset
//! field buttons, the shared indicator and make unique, curve and gradient fields, the entity
//! eyedropper, and the five agent methods.

use regia_ecs::reflect::Reflect;
use regia_render::MeshSource;
use regia_scene::prelude::{Name, Transform};
use serde_json::{Value, json};

use super::*;

/// A key of a test curve (the curve shape with tangents).
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Default)]
struct FalloffKey {
    t: f32,
    v: f32,
    in_tangent: f32,
    out_tangent: f32,
}

/// A stop of a test gradient.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Default)]
struct RampStop {
    t: f32,
    color: [f32; 4],
}

/// A component with a curve, a gradient and an entity reference.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
struct Falloff {
    curve: Vec<FalloffKey>,
    ramp: Vec<RampStop>,
    target: Entity,
}

impl Default for Falloff {
    fn default() -> Self {
        Self {
            curve: vec![
                FalloffKey {
                    t: 0.0,
                    v: 1.0,
                    ..FalloffKey::default()
                },
                FalloffKey {
                    t: 2.0,
                    v: 0.0,
                    ..FalloffKey::default()
                },
            ],
            ramp: vec![RampStop {
                t: 0.0,
                color: [1.0, 1.0, 1.0, 1.0],
            }],
            target: Entity::PLACEHOLDER,
        }
    }
}

fn app() -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<MeshSource>();
    app.register_type::<Falloff>();
    app.init_resource::<UndoStack>();
    app
}

fn select(world: &mut World, entity: Entity) {
    let mut selection = Selection::default();
    selection.select(entity, crate::selection::SelectMode::Replace);
    world.insert_resource(selection);
}

fn undo_depth(world: &World) -> usize {
    world.resource::<UndoStack>().undo_depth()
}

/// One inspector frame with `events`; returns the AccessKit tree.
fn frame(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>) -> egui::accesskit::TreeUpdate {
    ctx.enable_accesskit();
    ctx.all_styles_mut(|style| style.animation_time = 0.0);
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1100.0, 1600.0),
        )),
        events,
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| inspector_panel(ui, world));
    output.textures_delta.clear();
    output
        .platform_output
        .accesskit_update
        .unwrap_or_else(|| egui::accesskit::TreeUpdate {
            nodes: Vec::new(),
            tree: None,
            focus: egui::accesskit::NodeId(0),
            tree_id: egui::accesskit::TreeId::ROOT,
        })
}

fn node_center(update: &egui::accesskit::TreeUpdate, text: &str) -> Option<egui::Pos2> {
    update.nodes.iter().find_map(|(_, node)| {
        let named = node.label() == Some(text) || node.value() == Some(text);
        let bounds = node.bounds()?;
        named.then(|| {
            egui::pos2(
                ((bounds.x0 + bounds.x1) / 2.0) as f32,
                ((bounds.y0 + bounds.y1) / 2.0) as f32,
            )
        })
    })
}

fn labels(update: &egui::accesskit::TreeUpdate) -> Vec<String> {
    update
        .nodes
        .iter()
        .filter_map(|(_, node)| node.label().map(str::to_owned))
        .collect()
}

fn focused_label(update: &egui::accesskit::TreeUpdate) -> Option<String> {
    update
        .nodes
        .iter()
        .find(|(id, _)| *id == update.focus)
        .and_then(|(_, node)| node.label().map(str::to_owned))
}

fn keys(key: egui::Key) -> Vec<egui::Event> {
    [true, false]
        .into_iter()
        .map(|pressed| egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        })
        .collect()
}

/// Opens the component header called `header` with a click.
fn open_header(ctx: &egui::Context, world: &mut World, header: &str) {
    let tree = frame(ctx, world, Vec::new());
    let at = node_center(&tree, header).unwrap_or_else(|| panic!("{header}: {:?}", labels(&tree)));
    let press = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    frame(ctx, world, vec![egui::Event::PointerMoved(at), press(true)]);
    frame(ctx, world, vec![press(false)]);
    frame(
        ctx,
        world,
        vec![egui::Event::PointerMoved(egui::pos2(1090.0, 1590.0))],
    );
}

/// Presses Tab until the focused widget is called `label` (the keyboard path), then Enter.
fn tab_and_enter(ctx: &egui::Context, world: &mut World, label: &str) {
    let mut seen = Vec::new();
    for _ in 0..120 {
        let tree = frame(ctx, world, keys(egui::Key::Tab));
        let focus = focused_label(&tree);
        if focus.as_deref() == Some(label) {
            frame(ctx, world, keys(egui::Key::Enter));
            frame(ctx, world, Vec::new());
            return;
        }
        seen.push(focus.unwrap_or_default());
    }
    panic!("Tab never reached {label:?}; focused in turn: {seen:?}");
}

/// Use selected, Clear and Browse to on an asset field, all from the keyboard; each edit is one
/// undo step and a field never takes an asset of the wrong kind.
#[test]
fn asset_field_buttons_work_from_the_keyboard() {
    let mut app = app();
    let world = app.world_mut();
    let entity = world
        .spawn((
            Name::new("Crate"),
            Transform::default(),
            MeshSource::new("assets/props/crate.glb", ""),
        ))
        .id();
    select(world, entity);
    world.insert_resource(picker_links::ContentSelection(Some(
        "assets/materials/wet.matgraph.ron".to_owned(),
    )));
    let ctx = egui::Context::default();
    open_header(&ctx, world, "MeshSource");
    let before = undo_depth(world);
    tab_and_enter(&ctx, world, "Use selected asset for MeshSource.material");
    assert_eq!(
        world.get::<MeshSource>(entity).map(|m| m.material.clone()),
        Some("assets/materials/wet.matgraph.ron".to_owned())
    );
    assert_eq!(undo_depth(world), before + 1, "one undo step");
    // The mesh field does not take a material: its Use selected stays off (focus skips it).
    assert_eq!(
        world.get::<MeshSource>(entity).map(|m| m.mesh.clone()),
        Some("assets/props/crate.glb".to_owned())
    );
    tab_and_enter(&ctx, world, "Clear MeshSource.material");
    assert_eq!(
        world.get::<MeshSource>(entity).map(|m| m.material.clone()),
        Some(String::new())
    );
    assert_eq!(undo_depth(world), before + 2);
    tab_and_enter(&ctx, world, "Browse to MeshSource.mesh");
    let revealed = world
        .get_resource_mut::<crate::asset_panels::AssetPanels>()
        .and_then(|mut panels| panels.take_selected());
    assert_eq!(revealed.as_deref(), Some("assets/props/crate.glb"));
}

/// A material two levels use shows the shared indicator with the count (counted once, not per
/// frame), and Make unique copies it next to the asset and repoints the field as one undo step.
#[test]
fn a_shared_asset_shows_its_count_and_makes_unique() {
    let dir = regia_test_utils::unique_temp_dir("inspector-make-unique");
    std::fs::create_dir_all(dir.join("assets/materials")).expect("dir");
    std::fs::create_dir_all(dir.join("assets/levels")).expect("dir");
    std::fs::write(
        dir.join("assets/materials/rock.material.ron"),
        "(base_color: (1.0, 1.0, 1.0, 1.0))",
    )
    .expect("material");
    for level in ["a", "b"] {
        std::fs::write(
            dir.join(format!("assets/levels/{level}.level.ron")),
            "(entities: [(components: { \"MeshSource\": (mesh: \"builtin:cube\", material: \"assets/materials/rock.material.ron\") })])",
        )
        .expect("level");
    }
    let mut app = app();
    let world = app.world_mut();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    let entity = world
        .spawn((
            Name::new("Rock"),
            Transform::default(),
            MeshSource::new("builtin:cube", "assets/materials/rock.material.ron"),
        ))
        .id();
    select(world, entity);
    let ctx = egui::Context::default();
    open_header(&ctx, world, "MeshSource");
    let tree = frame(&ctx, world, Vec::new());
    assert!(
        node_center(&tree, "MeshSource.material is shared by 2 files").is_some(),
        "{:?}",
        labels(&tree)
    );
    for _ in 0..5 {
        frame(&ctx, world, Vec::new());
    }
    let computed = world.resource::<picker_links::ReferencerCache>().computed;
    assert_eq!(
        computed, 1,
        "one count per content-index generation, not per frame"
    );
    let before = undo_depth(world);
    tab_and_enter(&ctx, world, "Make MeshSource.material unique");
    let material = world.get::<MeshSource>(entity).map(|m| m.material.clone());
    assert_eq!(
        material.as_deref(),
        Some("assets/materials/rock_unique.material.ron")
    );
    assert!(dir.join("assets/materials/rock_unique.material.ron").is_file());
    assert!(
        !dir.join("assets/materials/rock_unique.material.ron.meta")
            .exists()
    );
    assert_eq!(undo_depth(world), before + 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A curve field opens from the keyboard and Add key adds a key as one undo step; a gradient
/// preset replaces the stops.
#[test]
fn curve_and_gradient_fields_edit_from_the_keyboard() {
    let mut app = app();
    let world = app.world_mut();
    let entity = world.spawn((Name::new("Lamp"), Falloff::default())).id();
    select(world, entity);
    let ctx = egui::Context::default();
    open_header(&ctx, world, "Falloff");
    let before = undo_depth(world);
    tab_and_enter(&ctx, world, "curve curve");
    tab_and_enter(&ctx, world, "Add curve key");
    let curve = world
        .get::<Falloff>(entity)
        .map(|f| f.curve.clone())
        .unwrap_or_default();
    assert_eq!(curve.len(), 3, "{curve:?}");
    assert_eq!(curve[2].t, 2.5, "a quarter of the span after the last key");
    assert_eq!(undo_depth(world), before + 1);
    tab_and_enter(&ctx, world, "ramp gradient");
    tab_and_enter(&ctx, world, "ramp preset Fade out");
    let ramp = world
        .get::<Falloff>(entity)
        .map(|f| f.ramp.clone())
        .unwrap_or_default();
    assert_eq!(
        ramp,
        vec![
            RampStop {
                t: 0.0,
                color: [1.0, 1.0, 1.0, 1.0]
            },
            RampStop {
                t: 1.0,
                color: [1.0, 1.0, 1.0, 0.0]
            }
        ]
    );
}

/// The entity eyedropper: armed from the keyboard, the next entity picked (a selection change,
/// as a viewport or Outliner click makes) goes into the field as one undo step and the selection
/// goes back; Esc disarms it.
#[test]
fn the_entity_eyedropper_takes_the_next_pick_and_esc_cancels() {
    let mut app = app();
    let world = app.world_mut();
    let entity = world.spawn((Name::new("Lamp"), Falloff::default())).id();
    let other = world.spawn((Name::new("Target"), Transform::default())).id();
    select(world, entity);
    let ctx = egui::Context::default();
    open_header(&ctx, world, "Falloff");
    tab_and_enter(&ctx, world, "Eyedropper for Falloff.target");
    assert!(world.get_resource::<picker_links::EntityEyedropper>().is_some());
    frame(&ctx, world, keys(egui::Key::Escape));
    assert!(
        world.get_resource::<picker_links::EntityEyedropper>().is_none(),
        "Esc cancels"
    );
    tab_and_enter(&ctx, world, "Eyedropper for Falloff.target");
    let before = undo_depth(world);
    select(world, other);
    frame(&ctx, world, Vec::new());
    assert_eq!(world.get::<Falloff>(entity).map(|f| f.target), Some(other));
    assert_eq!(undo_depth(world), before + 1);
    assert_eq!(
        world.resource::<Selection>().primary(),
        Some(entity),
        "the selection goes back"
    );
    assert!(world.get_resource::<picker_links::EntityEyedropper>().is_none());
}

fn call(
    app: &mut regia_app::App,
    method: &str,
    params: Value,
) -> Result<Value, regia_agent::protocol::RpcError> {
    let handler = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .map(|(_, handler)| handler)
        .unwrap_or_else(|| panic!("{method} is registered"));
    handler(app.world_mut(), &params)
}

fn reason(e: &regia_agent::protocol::RpcError) -> String {
    e.data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// The five methods through the editor's method table, with their refusals.
#[test]
fn the_picker_methods_dispatch() {
    let dir = regia_test_utils::unique_temp_dir("inspector-picker-methods");
    std::fs::create_dir_all(dir.join("assets/materials")).expect("dir");
    std::fs::write(dir.join("assets/materials/rock.material.ron"), "()").expect("material");
    std::fs::write(
        dir.join("assets/materials/user.matinst.ron"),
        "(parent: \"assets/materials/rock.material.ron\")",
    )
    .expect("user");
    let mut app = app();
    app.world_mut().insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    let entity = app
        .world_mut()
        .spawn((MeshSource::new("builtin:cube", ""), Falloff::default()))
        .id();
    let bits = entity.to_bits();

    // asset_field_set: a path, the Content Browser selection, a refusal, and the bad calls.
    let v = call(
        &mut app,
        "editor.asset_field_set",
        json!({ "entity": bits, "component": "MeshSource", "field": "material",
                "path": "assets/materials/rock.material.ron" }),
    )
    .expect("set");
    assert_eq!(v["after"], "assets/materials/rock.material.ron");
    assert_eq!(v["changed"], true);
    let set = |app: &mut regia_app::App, params: Value| call(app, "editor.asset_field_set", params);
    let e = set(
        &mut app,
        json!({ "entity": bits, "component": "MeshSource", "field": "material", "from_selection": true }),
    )
    .expect_err("no selection");
    assert_eq!(reason(&e), "asset_field_no_selection");
    app.world_mut()
        .insert_resource(picker_links::ContentSelection(Some(
            "assets/props/crate.glb".to_owned(),
        )));
    let e = set(
        &mut app,
        json!({ "entity": bits, "component": "MeshSource", "field": "material", "from_selection": true }),
    )
    .expect_err("wrong kind");
    assert_eq!(reason(&e), "asset_field_refused");
    let v = set(
        &mut app,
        json!({ "entity": bits, "component": "MeshSource", "field": "mesh", "from_selection": true }),
    )
    .expect("mesh from selection");
    assert_eq!(v["after"], "assets/props/crate.glb");
    for params in [
        json!({ "entity": bits, "component": "MeshSource", "field": "mesh" }),
        json!({ "entity": bits, "component": "MeshSource", "field": "mesh", "path": "", "from_selection": true }),
    ] {
        assert_eq!(
            reason(&set(&mut app, params).expect_err("bad")),
            "asset_field_bad_params"
        );
    }

    // asset_referencer_count: the instance references the material.
    let v = call(
        &mut app,
        "editor.asset_referencer_count",
        json!({ "path": "materials/rock.material.ron" }),
    )
    .expect("count");
    assert_eq!(v["path"], "assets/materials/rock.material.ron");
    assert_eq!(v["count"], 1);
    assert_eq!(v["shared"], false);
    assert_eq!(v["referencers"], json!(["assets/materials/user.matinst.ron"]));
    let e = call(
        &mut app,
        "editor.asset_referencer_count",
        json!({ "path": "builtin:cube" }),
    )
    .expect_err("builtin");
    assert_eq!(reason(&e), "asset_path_invalid");

    // make_unique: a copy and a repoint; a built-in refuses.
    let v = call(
        &mut app,
        "editor.make_unique",
        json!({ "entity": bits, "component": "MeshSource", "path": "material" }),
    )
    .expect("unique");
    assert_eq!(v["to"], "assets/materials/rock_unique.material.ron");
    assert!(dir.join("assets/materials/rock_unique.material.ron").is_file());
    app.world_mut().get_mut::<MeshSource>(entity).expect("mesh").mesh = "builtin:cube".to_owned();
    let e = call(
        &mut app,
        "editor.make_unique",
        json!({ "entity": bits, "component": "MeshSource", "path": "mesh" }),
    )
    .expect_err("builtin");
    assert_eq!(reason(&e), "make_unique_builtin");

    // curve_field_edit: read, replace, preset, refusals.
    let curve = |app: &mut regia_app::App, extra: Value| {
        let mut params = json!({ "entity": bits, "component": "Falloff", "path": "curve" });
        if let (Some(p), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            p.extend(extra.clone());
        }
        call(app, "editor.curve_field_edit", params)
    };
    let v = curve(&mut app, json!({})).expect("read");
    assert_eq!(v["kind"], "curve");
    assert_eq!(v["keys"].as_array().map(Vec::len), Some(2));
    let v = curve(
        &mut app,
        json!({ "keys": [{ "t": 1.0, "v": 3.0 }, { "t": -1.0, "v": 2.0, "in": 0.5, "out": 0.5 }] }),
    )
    .expect("keys");
    assert_eq!(
        v["keys"][0],
        json!({ "t": -1.0, "v": 2.0, "in": 0.5, "out": 0.5 }),
        "sorted by t"
    );
    assert_eq!(
        app.world().get::<Falloff>(entity).map(|f| f.curve[1].v),
        Some(3.0)
    );
    let v = curve(&mut app, json!({ "preset": "linear up" })).expect("preset");
    assert_eq!(v["keys"][1]["v"], 1.0);
    assert_eq!(
        reason(&curve(&mut app, json!({ "preset": "nope" })).expect_err("preset")),
        "curve_preset_unknown"
    );
    assert_eq!(
        reason(&curve(&mut app, json!({ "keys": [{ "t": 0.0 }] })).expect_err("key")),
        "curve_key_invalid"
    );
    let v = call(
        &mut app,
        "editor.curve_field_edit",
        json!({ "entity": bits, "component": "Falloff", "path": "ramp",
                "keys": [{ "t": 0.0, "color": "#ff0000" }, { "t": 1.0, "color": [0.0, 0.0, 1.0, 0.5] }] }),
    )
    .expect("gradient");
    assert_eq!(v["kind"], "gradient");
    assert_eq!(v["keys"][0]["hex"], "#ff0000ff");
    let e = call(
        &mut app,
        "editor.curve_field_edit",
        json!({ "entity": bits, "component": "Falloff", "path": "target" }),
    )
    .expect_err("not a curve");
    assert_eq!(reason(&e), "curve_field_not_curve");

    // color_swatches: add, remove, clear, refusals.
    let swatch = |app: &mut regia_app::App, params: Value| call(app, "editor.color_swatches", params);
    swatch(&mut app, json!({ "add": "#00ff00" })).expect("add");
    let v = swatch(&mut app, json!({ "add": [1.0, 0.0, 0.0] })).expect("add");
    assert_eq!(v["swatches"], json!(["#ff0000ff", "#00ff00ff"]));
    let v = swatch(&mut app, json!({ "remove": 0 })).expect("remove");
    assert_eq!(v["swatches"], json!(["#00ff00ff"]));
    assert_eq!(
        reason(&swatch(&mut app, json!({ "remove": 5 })).expect_err("range")),
        "swatch_index_out_of_range"
    );
    assert_eq!(
        reason(&swatch(&mut app, json!({ "add": "#zz" })).expect_err("color")),
        "color_invalid"
    );
    let v = swatch(&mut app, json!({ "clear": true })).expect("clear");
    assert_eq!(v["swatches"], json!([]));
    let _ = std::fs::remove_dir_all(&dir);
}
