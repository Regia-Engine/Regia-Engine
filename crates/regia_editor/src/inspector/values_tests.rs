// SPDX-License-Identifier: MPL-2.0
//! Inspector value widgets driven through egui input (UI parity addendum § 7.7-7.9, ADR-7785):
//! typed expressions with units, drag modifiers and label scrubbing, linked vector components,
//! flags and the bit-layer grid, file pickers, inspector buttons, and their agent routes.

use regia_ecs::reflect::std_traits::ReflectDefault;
use regia_ecs::reflect::{PartialReflect, Reflect, TypeRegistry};
use serde_json::{Value, json};

use super::actions::{ComponentAction, ComponentActions};
use super::impls::{PathPick, display_number, file_filter_matches, last_expr_refusal, project_paths};
use super::options::NumberOptions;
use super::*;

/// One frame of `draw` with `events`; returns the AccessKit tree.
fn frame(
    ctx: &egui::Context,
    events: Vec<egui::Event>,
    draw: &mut dyn FnMut(&mut egui::Ui),
) -> egui::accesskit::TreeUpdate {
    ctx.enable_accesskit();
    ctx.all_styles_mut(|style| style.animation_time = 0.0);
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(900.0, 700.0),
        )),
        events,
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, draw);
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

fn center(node: &egui::accesskit::Node) -> Option<egui::Pos2> {
    let b = node.bounds()?;
    Some(egui::pos2(
        ((b.x0 + b.x1) / 2.0) as f32,
        ((b.y0 + b.y1) / 2.0) as f32,
    ))
}

/// The center of the node labeled (or valued) `text`: a label widget carries its text as value.
fn find(update: &egui::accesskit::TreeUpdate, text: &str) -> Option<egui::Pos2> {
    update.nodes.iter().find_map(|(_, node)| {
        (node.label() == Some(text) || node.value() == Some(text))
            .then(|| center(node))
            .flatten()
    })
}

/// The centers of the spin buttons (drag values) in reading order: top to bottom, then left to
/// right (the tree's node order is not the screen's).
fn spinners(update: &egui::accesskit::TreeUpdate) -> Vec<egui::Pos2> {
    let mut out: Vec<egui::Pos2> = update
        .nodes
        .iter()
        .filter(|(_, node)| node.role() == egui::accesskit::Role::SpinButton)
        .filter_map(|(_, node)| center(node))
        .collect();
    out.sort_by(|a, b| {
        (a.y.round(), a.x)
            .partial_cmp(&(b.y.round(), b.x))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn labels(update: &egui::accesskit::TreeUpdate) -> Vec<String> {
    update
        .nodes
        .iter()
        .flat_map(|(_, node)| [node.label(), node.value()])
        .flatten()
        .map(str::to_owned)
        .collect()
}

fn button(pos: egui::Pos2, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers,
    }
}

fn key(key: egui::Key) -> Vec<egui::Event> {
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

/// A click at `pos` over two frames.
fn click(ctx: &egui::Context, pos: egui::Pos2, draw: &mut dyn FnMut(&mut egui::Ui)) {
    frame(
        ctx,
        vec![
            egui::Event::PointerMoved(pos),
            button(pos, true, egui::Modifiers::NONE),
        ],
        draw,
    );
    frame(ctx, vec![button(pos, false, egui::Modifiers::NONE)], draw);
}

/// Clicks the drag value at `pos` into text mode, types `text`, presses Enter.
fn type_into(ctx: &egui::Context, pos: egui::Pos2, text: &str, draw: &mut dyn FnMut(&mut egui::Ui)) {
    click(ctx, pos, draw);
    frame(ctx, Vec::new(), draw);
    frame(ctx, vec![egui::Event::Text(text.to_owned())], draw);
    frame(ctx, key(egui::Key::Enter), draw);
    frame(ctx, Vec::new(), draw);
}

/// A primary-button drag from `from` by `steps` (one frame each), holding `modifiers`.
fn drag(
    ctx: &egui::Context,
    from: egui::Pos2,
    steps: &[egui::Vec2],
    modifiers: egui::Modifiers,
    draw: &mut dyn FnMut(&mut egui::Ui),
) {
    frame(
        ctx,
        vec![
            egui::Event::ModifiersChanged(modifiers),
            egui::Event::PointerMoved(from),
        ],
        draw,
    );
    frame(ctx, vec![button(from, true, modifiers)], draw);
    let mut at = from;
    for step in steps {
        at += *step;
        frame(ctx, vec![egui::Event::PointerMoved(at)], draw);
    }
    frame(ctx, vec![button(at, false, modifiers)], draw);
    frame(
        ctx,
        vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)],
        draw,
    );
}

/// Presses Tab until the focused node is labeled `label` (at most 30 times).
fn tab_to(ctx: &egui::Context, label: &str, draw: &mut dyn FnMut(&mut egui::Ui)) -> bool {
    for _ in 0..30 {
        let tree = frame(ctx, key(egui::Key::Tab), draw);
        let focused = tree
            .nodes
            .iter()
            .find(|(id, _)| *id == tree.focus)
            .and_then(|(_, node)| node.label().map(str::to_owned));
        if focused.as_deref() == Some(label) {
            return true;
        }
    }
    false
}

/// Draws `value` (inside component `component`) with a fresh drawer over `registry`.
fn draw_value(
    ui: &mut egui::Ui,
    registry: &TypeRegistry,
    context: &mut InspectorContext,
    value: &mut dyn PartialReflect,
) {
    InspectorUi::new(registry, context).ui_for_reflect_with_options(value, ui, egui::Id::new("values"), &());
}

#[derive(Reflect, Default, Debug, Clone, PartialEq, Component)]
#[reflect(Component, Default)]
struct Mover {
    #[reflect(@regia_ecs::attrs::Units("m"))]
    reach: f32,
    speed: f32,
    count: i32,
}

/// Item 1: typed expressions apply on commit, in the field's unit, relative to the current value;
/// a refused one keeps the value and reports why.
#[test]
fn typed_expressions_apply_with_units_and_relative_edits() {
    let mut registry = TypeRegistry::default();
    registry.register::<Mover>();
    let ctx = egui::Context::default();
    let mut mover = Mover {
        reach: 1.0,
        speed: 0.0,
        count: 3,
    };
    let mut context = InspectorContext::new("game::Mover");
    let mut draw = |ui: &mut egui::Ui| draw_value(ui, &registry, &mut context, &mut mover);
    let tree = frame(&ctx, Vec::new(), &mut draw);
    let fields = spinners(&tree);
    assert_eq!(fields.len(), 3, "{:?}", labels(&tree));
    type_into(&ctx, fields[0], "5cm", &mut draw);
    type_into(&ctx, fields[1], "2*pi", &mut draw);
    type_into(&ctx, fields[2], "*=4", &mut draw);
    drop(draw);
    assert!(
        (mover.reach - 0.05).abs() < 1e-6,
        "5cm in a meter field: {}",
        mover.reach
    );
    assert!(
        (mover.speed - std::f32::consts::TAU).abs() < 1e-5,
        "2*pi: {}",
        mover.speed
    );
    assert_eq!(mover.count, 12, "*=4 of 3");
    let paths: Vec<String> = context.take_changes().into_iter().map(|c| c.path).collect();
    assert_eq!(paths, ["reach", "speed", "count"]);

    let mut draw = |ui: &mut egui::Ui| draw_value(ui, &registry, &mut context, &mut mover);
    type_into(&ctx, fields[0], "+=2", &mut draw);
    type_into(&ctx, fields[1], "2+", &mut draw);
    drop(draw);
    assert!((mover.reach - 2.05).abs() < 1e-5, "+=2: {}", mover.reach);
    assert!(
        (mover.speed - std::f32::consts::TAU).abs() < 1e-5,
        "a refused expression keeps the value"
    );
    assert!(
        last_expr_refusal(&ctx).is_some_and(|m| m.contains("ends too early")),
        "{:?}",
        last_expr_refusal(&ctx)
    );
}

/// Item 2: Ctrl snaps a drag to the step, Alt drags ten times faster than a plain drag.
#[test]
fn ctrl_snaps_and_alt_speeds_up_a_drag() {
    let ctx = egui::Context::default();
    let options = NumberOptions::<f64> {
        speed: 0.5,
        ..NumberOptions::default()
    };
    let run = |modifiers: egui::Modifiers| {
        let ctx = egui::Context::default();
        let mut value = 0.0f64;
        let mut draw = |ui: &mut egui::Ui| {
            display_number(&mut value, &options, ui);
        };
        let tree = frame(&ctx, Vec::new(), &mut draw);
        let field = spinners(&tree)[0];
        drag(
            &ctx,
            field,
            &[egui::vec2(10.0, 0.0), egui::vec2(3.0, 0.0)],
            modifiers,
            &mut draw,
        );
        drop(draw);
        value
    };
    let plain = run(egui::Modifiers::NONE);
    let fast = run(egui::Modifiers::ALT);
    let snapped = run(egui::Modifiers::COMMAND);
    assert!(plain > 0.0, "a drag moves the value: {plain}");
    assert!(fast > plain * 5.0, "Alt drags faster: plain {plain}, alt {fast}");
    assert!(snapped > 0.0, "{snapped}");
    assert!(
        (snapped / 0.5 - (snapped / 0.5).round()).abs() < 1e-9,
        "Ctrl snaps to the 0.5 step: {snapped}"
    );
    drop(ctx);
    assert_eq!(super::impls::snap_to_step(1.26, 0.25), 1.25);
    assert_eq!(super::impls::snap_step(0.0, true), 1.0);
    assert_eq!(super::impls::modified_speed(0.5, egui::Modifiers::ALT), 5.0);
}

/// Item 2: a drag over a number field's label scrubs it like a drag over the field.
#[test]
fn dragging_a_field_label_scrubs_the_number() {
    let mut registry = TypeRegistry::default();
    registry.register::<Mover>();
    let ctx = egui::Context::default();
    let mut mover = Mover::default();
    let mut context = InspectorContext::new("game::Mover");
    let mut draw = |ui: &mut egui::Ui| draw_value(ui, &registry, &mut context, &mut mover);
    let tree = frame(&ctx, Vec::new(), &mut draw);
    let label = find(&tree, "speed").unwrap_or_else(|| panic!("speed label: {:?}", labels(&tree)));
    drag(
        &ctx,
        label,
        &[
            egui::vec2(10.0, 0.0),
            egui::vec2(20.0, 0.0),
            egui::vec2(10.0, 0.0),
        ],
        egui::Modifiers::NONE,
        &mut draw,
    );
    drop(draw);
    assert!(
        mover.speed > 0.5,
        "the label drag scrubbed speed: {}",
        mover.speed
    );
    assert_eq!(mover.reach, 0.0, "only the dragged field moves");
    assert!(context.take_changes().iter().any(|c| c.path == "speed"));
    let scrubbed = super::impls::scrub_value(1.0, 10.0, 0.0, egui::Modifiers::NONE, false, (None, Some(1.2)));
    assert_eq!(scrubbed, 1.2, "clamped to the range");
}

#[derive(Reflect, Default, Debug, Clone, PartialEq, Component)]
#[reflect(Component, Default)]
struct Crate3 {
    scale: Vec3,
}

/// Item 3: with the chain toggle on, editing one axis scales the others by the same ratio; the
/// toggle is a named, keyboard-reachable control whose state goes to the preferences.
#[test]
fn linked_components_scale_together_and_the_toggle_is_remembered() {
    let mut registry = TypeRegistry::default();
    registry.register::<Crate3>();
    let ctx = egui::Context::default();
    let mut value = Crate3 {
        scale: Vec3::new(1.0, 2.0, 4.0),
    };
    let mut context = InspectorContext::new("game::Crate3").with_linked(["Crate3.scale"]);
    let mut draw = |ui: &mut egui::Ui| draw_value(ui, &registry, &mut context, &mut value);
    let tree = frame(&ctx, Vec::new(), &mut draw);
    let lanes = spinners(&tree);
    assert_eq!(lanes.len(), 3);
    type_into(&ctx, lanes[0], "2", &mut draw);
    drop(draw);
    assert_eq!(value.scale, Vec3::new(2.0, 4.0, 8.0), "uniform scale");
    let mut paths: Vec<String> = context.take_changes().into_iter().map(|c| c.path).collect();
    paths.sort();
    assert_eq!(paths, ["scale.x", "scale.y", "scale.z"]);

    // Unlink from the keyboard: Tab to the toggle, Space.
    let mut draw = |ui: &mut egui::Ui| draw_value(ui, &registry, &mut context, &mut value);
    assert!(
        tab_to(&ctx, "Unlink components", &mut draw),
        "the toggle takes keyboard focus"
    );
    frame(&ctx, key(egui::Key::Space), &mut draw);
    frame(&ctx, Vec::new(), &mut draw);
    drop(draw);
    assert_eq!(
        context.take_link_toggles(),
        vec![("Crate3.scale".to_owned(), false)]
    );

    // The preference keeps the keys sorted and without repeats.
    let mut world = World::new();
    world.insert_resource(crate::editor_prefs::EditorPreferences::default());
    store_link_toggles(
        &mut world,
        vec![("Transform.scale".into(), true), ("Decal.size".into(), true)],
    );
    assert_eq!(linked_vectors(&world), ["Decal.size", "Transform.scale"]);
    store_link_toggles(&mut world, vec![("Decal.size".into(), false)]);
    assert_eq!(linked_vectors(&world), ["Transform.scale"]);
}

#[derive(Reflect, Default, Debug, Clone, PartialEq, Component)]
#[reflect(Component, Default)]
struct Door {
    #[reflect(@regia_ecs::attrs::Flags("locked, jammed, alarmed"))]
    state: u8,
    #[reflect(@regia_ecs::attrs::LayerMask("render"))]
    seen_by: u8,
}

/// Item 4: `@Flags` draws a checkbox per named bit (keyboard operable); `@LayerMask` draws the
/// grid, where a press and a drag across cells paints them all, and Space flips a focused cell.
#[test]
fn flags_are_checkboxes_and_layer_masks_paint_by_drag() {
    let mut registry = TypeRegistry::default();
    registry.register::<Door>();
    let ctx = egui::Context::default();
    let mut door = Door::default();
    let mut context = InspectorContext::new("game::Door");
    let mut draw = |ui: &mut egui::Ui| draw_value(ui, &registry, &mut context, &mut door);
    let tree = frame(&ctx, Vec::new(), &mut draw);
    let names = labels(&tree);
    for name in [
        "state: locked",
        "state: jammed",
        "state: alarmed",
        "seen_by: Layer 0",
        "seen_by: Layer 7",
    ] {
        assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
    }
    assert!(tab_to(&ctx, "state: jammed", &mut draw));
    frame(&ctx, key(egui::Key::Space), &mut draw);
    frame(&ctx, Vec::new(), &mut draw);
    let tree = frame(&ctx, Vec::new(), &mut draw);
    let cell =
        |bit: u32| find(&tree, &format!("seen_by: Layer {bit}")).unwrap_or_else(|| panic!("cell {bit}"));
    let (c0, c1, c2) = (cell(0), cell(1), cell(2));
    drag(&ctx, c0, &[c1 - c0, c2 - c1], egui::Modifiers::NONE, &mut draw);
    assert!(tab_to(&ctx, "seen_by: Layer 5", &mut draw));
    frame(&ctx, key(egui::Key::Space), &mut draw);
    frame(&ctx, Vec::new(), &mut draw);
    drop(draw);
    assert_eq!(door.state, 0b010, "Space on the focused jammed checkbox");
    assert_eq!(
        door.seen_by, 0b10_0111,
        "cells 0-2 painted on, cell 5 by keyboard"
    );
    let paths: Vec<String> = context.take_changes().into_iter().map(|c| c.path).collect();
    assert!(
        paths.contains(&"state".to_owned()) && paths.contains(&"seen_by".to_owned()),
        "{paths:?}"
    );
}

/// Item 5: the picker lists only the project's matching files (or folders), never hidden or
/// build folders, and never through a link out of the project.
#[test]
fn path_pickers_stay_inside_the_project() {
    let root = regia_test_utils::unique_temp_dir("w12_path_picker");
    let write = |rel: &str| {
        let path = root.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("dir");
        }
        std::fs::write(path, b"x").expect("file");
    };
    for rel in [
        "a.png",
        "b.jpg",
        "c.txt",
        "sub/d.PNG",
        ".git/e.png",
        "target/f.png",
    ] {
        write(rel);
    }
    #[cfg(unix)]
    {
        let outside = regia_test_utils::unique_temp_dir("w12_path_picker_outside");
        std::fs::write(outside.join("g.png"), b"x").expect("file");
        std::os::unix::fs::symlink(&outside, root.join("linked")).expect("symlink");
    }
    assert_eq!(
        project_paths(&root, PathPick::File("*.png;*.jpg")),
        ["a.png", "b.jpg", "sub/d.PNG"]
    );
    assert_eq!(project_paths(&root, PathPick::Dir), ["sub"]);
    assert_eq!(project_paths(&root, PathPick::File("")).len(), 4);
    assert!(file_filter_matches("png, jpg", "x/y.JPG"));
    assert!(!file_filter_matches("*.png", "x/png"));
    let _ = std::fs::remove_dir_all(&root);
}

#[derive(Reflect, Default, Debug, Clone, PartialEq, Component)]
#[reflect(Component, Default)]
struct Kiln {
    #[reflect(@regia_ecs::attrs::Button("Bake"))]
    bake: (),
    #[reflect(@regia_ecs::attrs::Units("m"))]
    heat: f32,
    #[reflect(@regia_ecs::attrs::FilePicker("*.png"))]
    decal: String,
}

fn kiln_app() -> (regia_app::App, Entity) {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<Kiln>();
    app.init_resource::<UndoStack>();
    let mut actions = ComponentActions::default();
    let bake: super::actions::ActionFn = std::sync::Arc::new(|world: &mut World, entity: Entity| {
        let mut kiln = world.get_mut::<Kiln>(entity).ok_or("no kiln")?;
        kiln.heat += 100.0;
        Ok(())
    });
    let crack: super::actions::ActionFn = std::sync::Arc::new(|world: &mut World, entity: Entity| {
        if let Some(mut kiln) = world.get_mut::<Kiln>(entity) {
            kiln.heat = -1.0;
        }
        Err("the kiln cracked".into())
    });
    for (id, label, run) in [("bake", "Bake", bake), ("crack", "Crack", crack)] {
        actions
            .register(
                "game",
                ComponentAction {
                    component: "Kiln".into(),
                    id: id.into(),
                    label: label.into(),
                    run,
                },
            )
            .expect("register");
    }
    app.insert_resource(actions);
    let entity = app.world_mut().spawn(Kiln::default()).id();
    (app, entity)
}

/// Item 6: a `@Button` field draws a button whose click asks for its action; running it is one
/// undo step; a failing action restores the component.
#[test]
fn inspector_buttons_run_their_action_with_undo() {
    let mut registry = TypeRegistry::default();
    registry.register::<Kiln>();
    let ctx = egui::Context::default();
    let mut kiln = Kiln::default();
    let mut context = InspectorContext::new("game::Kiln");
    let mut draw = |ui: &mut egui::Ui| draw_value(ui, &registry, &mut context, &mut kiln);
    assert!(tab_to(&ctx, "Bake", &mut draw), "the button takes keyboard focus");
    frame(&ctx, key(egui::Key::Enter), &mut draw);
    frame(&ctx, Vec::new(), &mut draw);
    drop(draw);
    assert_eq!(context.take_action_requests(), ["bake"]);
    assert!(context.take_changes().is_empty(), "a button edits no value");

    let (mut app, entity) = kiln_app();
    let world = app.world_mut();
    let run = super::actions::run_component_action(world, entity, "Kiln", "bake").expect("bake");
    assert!(run.changed);
    assert_eq!(world.get::<Kiln>(entity).map(|k| k.heat), Some(100.0));
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    assert!(crate::command::undo_world(world));
    assert_eq!(
        world.get::<Kiln>(entity).map(|k| k.heat),
        Some(0.0),
        "undo restores"
    );
    let err = super::actions::run_component_action(world, entity, "Kiln", "crack").expect_err("cracks");
    assert_eq!(
        (err.code, err.message.as_str()),
        ("action_failed", "the kiln cracked")
    );
    assert_eq!(
        world.get::<Kiln>(entity).map(|k| k.heat),
        Some(0.0),
        "a failed run is undone"
    );
    let err = super::actions::run_component_action(world, entity, "Kiln", "glaze").expect_err("unknown");
    assert_eq!(err.code, "action_unknown");
    let err = super::actions::run_component_action(world, entity, "Mover", "bake").expect_err("absent");
    assert_eq!(err.code, "component_absent");
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is registered"));
    handler(world, &params)
}

fn reason(err: &regia_agent::protocol::RpcError) -> Option<String> {
    let data = err.data.as_ref()?;
    data.get("reason")
        .or_else(|| data.get("code"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Agent surface: `editor.component_actions`, `editor.run_component_action` (undoable), and
/// `editor.set_property { expr }`, through the dispatcher's method table.
#[test]
fn agents_list_and_run_actions_and_set_numbers_by_expression() {
    let (mut app, entity) = kiln_app();
    let world = app.world_mut();
    let bits = entity.to_bits();
    let listed = call(world, "editor.component_actions", json!({ "entity": bits })).expect("list");
    let actions = listed["actions"].as_array().cloned().unwrap_or_default();
    let ids: Vec<(&str, &str, bool)> = actions
        .iter()
        .map(|a| {
            (
                a["action"].as_str().unwrap_or(""),
                a["source"].as_str().unwrap_or(""),
                a["runnable"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    assert!(ids.contains(&("bake", "button", true)), "{ids:?}");
    assert!(ids.contains(&("crack", "registered", true)), "{ids:?}");

    let ran = call(
        world,
        "editor.run_component_action",
        json!({ "entity": bits, "component": "Kiln", "action": "bake" }),
    )
    .expect("run");
    assert_eq!(ran["changed"], json!(true));
    assert_eq!(world.get::<Kiln>(entity).map(|k| k.heat), Some(100.0));
    let err = call(
        world,
        "editor.run_component_action",
        json!({ "entity": bits, "component": "Kiln", "action": "glaze" }),
    )
    .expect_err("unknown");
    assert_eq!(reason(&err).as_deref(), Some("action_unknown"));

    let set = |world: &mut World, params: Value| call(world, "editor.set_property", params);
    let out = set(
        world,
        json!({ "entity": bits, "component": "Kiln", "path": "heat", "expr": "50cm" }),
    )
    .expect("50cm");
    assert_eq!(out["after"], json!(0.5));
    set(
        world,
        json!({ "entity": bits, "component": "Kiln", "path": "heat", "expr": "*=4" }),
    )
    .expect("*=4");
    assert_eq!(world.get::<Kiln>(entity).map(|k| k.heat), Some(2.0));
    for (params, code) in [
        (
            json!({ "entity": bits, "component": "Kiln", "path": "heat", "expr": "2+" }),
            "expr_syntax",
        ),
        (
            json!({ "entity": bits, "component": "Kiln", "path": "heat", "expr": "3s" }),
            "expr_unit_mismatch",
        ),
        (
            json!({ "entity": bits, "component": "Kiln", "path": "decal", "expr": "2" }),
            "expr_not_number",
        ),
        (
            json!({ "entity": bits, "component": "Kiln", "path": "heat", "expr": "2", "value": 2.0 }),
            "value_and_expr",
        ),
        (
            json!({ "entities": [bits], "component": "Kiln", "path": "heat", "expr": "2" }),
            "expr_with_entities",
        ),
    ] {
        let err = set(world, params.clone()).expect_err("refused");
        assert_eq!(reason(&err).as_deref(), Some(code), "{params}: {err:?}");
    }
    assert_eq!(
        world.get::<Kiln>(entity).map(|k| k.heat),
        Some(2.0),
        "refusals change nothing"
    );
}
