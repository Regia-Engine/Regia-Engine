// SPDX-License-Identifier: MPL-2.0
//! w13 Inspector parity tests (ADR-8106): the multi-selection drawer's search, Modified only,
//! reset arrows, linked vectors and `@Button` actions (P-110, P-114, P-116, P-130), the color
//! eyedropper's keyboard and pointer paths (P-133) and the render mask's layer grid (P-132),
//! driven through egui input and read back from the AccessKit tree.

use std::collections::{BTreeMap, BTreeSet};

use regia_render::{Camera, PointLight};
use regia_scene::prelude::{Name, Transform};

use super::actions::{ComponentAction, ComponentActions};
use super::color_eyedropper::{self, ColorEyedropper};
use super::search::InspectorSearch;
use super::*;

/// A component with a `@Button` action, for the multi-selection run.
#[derive(Reflect, Default, Debug, Clone, PartialEq, Component)]
#[reflect(Component, Default)]
struct Kiln {
    #[reflect(@regia_ecs::attrs::Button("Bake"))]
    bake: (),
    heat: f32,
}

fn app() -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<PointLight>();
    app.register_type::<Camera>();
    app.register_type::<Kiln>();
    app.init_resource::<UndoStack>();
    let mut actions = ComponentActions::default();
    let bake: super::actions::ActionFn = std::sync::Arc::new(|world: &mut World, entity: Entity| {
        let mut kiln = world.get_mut::<Kiln>(entity).ok_or("no kiln")?;
        kiln.heat += 100.0;
        Ok(())
    });
    actions
        .register(
            "game",
            ComponentAction {
                component: "Kiln".into(),
                id: "bake".into(),
                label: "Bake".into(),
                run: bake,
            },
        )
        .expect("register");
    app.insert_resource(actions);
    app
}

fn select_all(world: &mut World, entities: &[Entity]) {
    let mut selection = Selection::default();
    for entity in entities {
        selection.select(*entity, crate::selection::SelectMode::Add);
    }
    world.insert_resource(selection);
}

fn lamp(world: &mut World, light: PointLight) -> Entity {
    world.spawn((Name::new("Lamp"), Transform::default(), light)).id()
}

/// One inspector frame with `events`; returns the AccessKit tree.
fn frame(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>) -> egui::accesskit::TreeUpdate {
    ctx.enable_accesskit();
    ctx.all_styles_mut(|style| style.animation_time = 0.0);
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(900.0, 1400.0),
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

fn texts(update: &egui::accesskit::TreeUpdate) -> Vec<String> {
    update
        .nodes
        .iter()
        .flat_map(|(_, node)| [node.label(), node.value()])
        .flatten()
        .map(str::to_owned)
        .collect()
}

fn has(update: &egui::accesskit::TreeUpdate, text: &str) -> bool {
    texts(update).iter().any(|t| t == text)
}

/// The centre of the first node labeled (or valued) `text`.
fn find(update: &egui::accesskit::TreeUpdate, text: &str) -> Option<egui::Pos2> {
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

/// The centres of every node labeled `text`.
fn find_all(update: &egui::accesskit::TreeUpdate, text: &str) -> Vec<egui::Pos2> {
    update
        .nodes
        .iter()
        .filter(|(_, node)| node.label() == Some(text))
        .filter_map(|(_, node)| {
            let bounds = node.bounds()?;
            Some(egui::pos2(
                ((bounds.x0 + bounds.x1) / 2.0) as f32,
                ((bounds.y0 + bounds.y1) / 2.0) as f32,
            ))
        })
        .collect()
}

fn press(pos: egui::Pos2, button: egui::PointerButton, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers: egui::Modifiers::default(),
    }
}

fn click(ctx: &egui::Context, world: &mut World, pos: egui::Pos2) {
    frame(
        ctx,
        world,
        vec![
            egui::Event::PointerMoved(pos),
            press(pos, egui::PointerButton::Primary, true),
        ],
    );
    frame(ctx, world, vec![press(pos, egui::PointerButton::Primary, false)]);
}

fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

/// Tabs (at most 200 times) until the keyboard focus is on the node labeled `label`.
fn tab_to(ctx: &egui::Context, world: &mut World, label: &str) -> bool {
    for _ in 0..200 {
        let tree = frame(ctx, world, vec![key(egui::Key::Tab, egui::Modifiers::NONE)]);
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

/// Ctrl+F with the pointer over the Inspector, then `text` typed.
fn type_search(ctx: &egui::Context, world: &mut World, text: &str) -> egui::accesskit::TreeUpdate {
    frame(
        ctx,
        world,
        vec![
            egui::Event::PointerMoved(egui::pos2(400.0, 600.0)),
            key(egui::Key::F, egui::Modifiers::COMMAND),
        ],
    );
    frame(ctx, world, vec![egui::Event::Text(text.to_owned())]);
    frame(ctx, world, Vec::new())
}

/// Opens the collapsing header labeled `header` (when it is closed).
fn open(ctx: &egui::Context, world: &mut World, header: &str, inside: &str) -> egui::accesskit::TreeUpdate {
    let tree = frame(ctx, world, Vec::new());
    if has(&tree, inside) {
        return tree;
    }
    let at = find(&tree, header).unwrap_or_else(|| panic!("{header}: {:?}", texts(&tree)));
    click(ctx, world, at);
    frame(ctx, world, Vec::new())
}

/// P-110 and P-116 on a multi-selection: Ctrl+F then "int" leaves only the intensity row of the
/// shared light; Modified only shows a field that differs on any selected entity and hides the
/// rest; a query that matches nothing says so.
#[test]
fn multi_selection_search_filters_rows() {
    let mut app = app();
    let world = app.world_mut();
    let first = lamp(
        world,
        PointLight {
            intensity: 900.0,
            ..PointLight::default()
        },
    );
    let second = lamp(
        world,
        PointLight {
            radius: 4.0,
            ..PointLight::default()
        },
    );
    select_all(world, &[first, second]);
    let ctx = egui::Context::default();
    let tree = frame(&ctx, world, Vec::new());
    assert!(has(&tree, "2 selected"), "{:?}", texts(&tree));
    assert!(
        has(&tree, "Search properties"),
        "the multi drawer has the search box"
    );

    let tree = type_search(&ctx, world, "int");
    assert_eq!(
        world.resource::<InspectorSearch>().query,
        "int",
        "Ctrl+F focused the box"
    );
    assert!(has(&tree, "intensity"), "int shows intensity: {:?}", texts(&tree));
    for hidden in ["radius", "color", "translation", "scale"] {
        assert!(
            !has(&tree, hidden),
            "{hidden} hidden under int: {:?}",
            texts(&tree)
        );
    }
    frame(&ctx, world, vec![key(egui::Key::A, egui::Modifiers::COMMAND)]);
    frame(&ctx, world, vec![egui::Event::Text("zzzq".into())]);
    let tree = frame(&ctx, world, Vec::new());
    assert!(has(&tree, "No field matches"), "{:?}", texts(&tree));
    frame(&ctx, world, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
    let tree = frame(&ctx, world, Vec::new());
    assert_eq!(
        world.resource::<InspectorSearch>().query,
        "",
        "Esc clears the search"
    );

    let toggle = find(&tree, "Modified only").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, toggle);
    assert!(world.resource::<InspectorSearch>().modified_only);
    let tree = frame(&ctx, world, Vec::new());
    assert!(
        has(&tree, "intensity"),
        "modified on the first lamp: {:?}",
        texts(&tree)
    );
    assert!(
        has(&tree, "radius"),
        "modified on the second lamp: {:?}",
        texts(&tree)
    );
    assert!(
        !has(&tree, "color"),
        "equal to its default on both: {:?}",
        texts(&tree)
    );
    assert!(
        !has(&tree, "Transform"),
        "default Transforms have no modified field: {:?}",
        texts(&tree)
    );
}

/// P-114 on a multi-selection: the reset arrow shows where any selected entity differs, and one
/// press (from the keyboard) resets the field on every entity as one undo step.
#[test]
fn multi_selection_reset_is_one_undo_step() {
    let mut app = app();
    let world = app.world_mut();
    let first = lamp(
        world,
        PointLight {
            intensity: 100.0,
            ..PointLight::default()
        },
    );
    let second = lamp(
        world,
        PointLight {
            intensity: 200.0,
            ..PointLight::default()
        },
    );
    select_all(world, &[first, second]);
    let ctx = egui::Context::default();
    let tree = open(&ctx, world, "PointLight", "intensity");
    assert!(
        has(&tree, "Reset intensity to default"),
        "the multi drawer shows the reset arrow: {:?}",
        texts(&tree)
    );
    assert!(
        !has(&tree, "Reset radius to default"),
        "radius is default on both"
    );
    let depth = world.resource::<UndoStack>().undo_depth();
    assert!(
        tab_to(&ctx, world, "Reset intensity to default"),
        "the arrow takes keyboard focus"
    );
    frame(&ctx, world, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
    frame(&ctx, world, Vec::new());
    let default = PointLight::default().intensity;
    for entity in [first, second] {
        assert_eq!(
            world.get::<PointLight>(entity).map(|l| l.intensity),
            Some(default)
        );
    }
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "one undo step"
    );
    assert!(crate::command::undo_world(world));
    assert_eq!(world.get::<PointLight>(first).map(|l| l.intensity), Some(100.0));
    assert_eq!(world.get::<PointLight>(second).map(|l| l.intensity), Some(200.0));
}

/// Linked vectors on a multi-selection (P-114 / ADR-7785): the link toggle is drawn and stored,
/// and a linked edit gives every entity the typed lane with its other lanes following by its own
/// ratio, as one undo step.
#[test]
fn multi_selection_linked_vectors_scale_each_entity_in_one_undo_step() {
    let mut app = app();
    let world = app.world_mut();
    let first = world.spawn((Name::new("A"), Transform::IDENTITY)).id();
    let second = world
        .spawn((
            Name::new("B"),
            Transform {
                scale: glam::Vec3::new(2.0, 4.0, 1.0),
                ..Transform::IDENTITY
            },
        ))
        .id();
    select_all(world, &[first, second]);
    let ctx = egui::Context::default();
    let tree = open(&ctx, world, "Transform", "scale");
    // Every vector field has a toggle (translation too): take the one on the scale row.
    let row = find(&tree, "scale").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    let toggle = find_all(&tree, "Link components (scale together)")
        .into_iter()
        .min_by(|a, b| (a.y - row.y).abs().total_cmp(&(b.y - row.y).abs()))
        .unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, toggle);
    assert!(
        linked_vectors(world).contains(&"Transform.scale".to_owned()),
        "the multi drawer stores the link"
    );

    // What the drawer records for a linked edit of A's x to 3: every lane of the sample moved.
    let depth = world.resource::<UndoStack>().undo_depth();
    let edited = Transform::from_scale(3.0);
    let changes: Vec<Change> = ["scale.x", "scale.y", "scale.z"]
        .iter()
        .map(|path| Change {
            path: (*path).to_owned(),
            discrete: false,
        })
        .collect();
    let linked: BTreeSet<String> = linked_vectors(world).into_iter().collect();
    multi::commit_drawer_changes_linked(
        world,
        &[first, second],
        "regia_scene::transform::Transform",
        &edited,
        &changes,
        Some(first),
        &BTreeMap::new(),
        &linked,
    )
    .expect("commit");
    assert_eq!(
        world.get::<Transform>(first).map(|t| t.scale),
        Some(glam::Vec3::splat(3.0))
    );
    assert_eq!(
        world.get::<Transform>(second).map(|t| t.scale),
        Some(glam::Vec3::new(3.0, 6.0, 1.5)),
        "x is typed, y and z follow B's own ratio"
    );
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "one undo step"
    );
    assert!(crate::command::undo_world(world));
    assert_eq!(
        world.get::<Transform>(second).map(|t| t.scale),
        Some(glam::Vec3::new(2.0, 4.0, 1.0))
    );
    assert_eq!(
        world.get::<Transform>(first).map(|t| t.scale),
        Some(glam::Vec3::ONE)
    );
}

/// P-130 on a multi-selection: the `@Button` runs its action on every selected entity (from the
/// keyboard), one undo step for all.
#[test]
fn multi_selection_buttons_run_on_every_entity() {
    let mut app = app();
    let world = app.world_mut();
    let first = world.spawn((Name::new("K1"), Kiln::default())).id();
    let second = world.spawn((Name::new("K2"), Kiln { bake: (), heat: 5.0 })).id();
    select_all(world, &[first, second]);
    let ctx = egui::Context::default();
    open(&ctx, world, "Kiln", "Bake");
    let depth = world.resource::<UndoStack>().undo_depth();
    assert!(tab_to(&ctx, world, "Bake"), "the button takes keyboard focus");
    frame(&ctx, world, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
    frame(&ctx, world, Vec::new());
    assert_eq!(world.get::<Kiln>(first).map(|k| k.heat), Some(100.0));
    assert_eq!(world.get::<Kiln>(second).map(|k| k.heat), Some(105.0));
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "one undo step"
    );
    assert!(crate::command::undo_world(world));
    assert_eq!(world.get::<Kiln>(first).map(|k| k.heat), Some(0.0));
    assert_eq!(world.get::<Kiln>(second).map(|k| k.heat), Some(5.0));
}

/// Answers the eyedropper's read-back with a window filled with `fill`.
fn serve_fill(world: &mut World, fill: [u8; 4]) {
    let callbacks = world
        .resource_mut::<regia_render::ScreenshotRequests>()
        .take_for(Entity::PLACEHOLDER);
    assert_eq!(callbacks.len(), 1, "the pick asked for one read-back");
    let (width, height) = (900u32, 1400u32);
    let image = regia_render::CapturedImage {
        width,
        height,
        rgba: fill.repeat((width * height) as usize),
    };
    for callback in callbacks {
        callback(Ok(image.clone()));
    }
}

/// P-133, keyboard path: Tab to the color's eyedropper button and press Enter to arm it, move the
/// ring with the arrow keys and press Enter to pick; the next frame after the read-back writes
/// the pixel (one undo step, remembered as a recent color). Esc cancels an armed eyedropper.
#[test]
fn the_eyedropper_is_operable_from_the_keyboard_and_esc_cancels() {
    let mut app = app();
    let world = app.world_mut();
    let first = lamp(world, PointLight::default());
    let mut selection = Selection::default();
    selection.select(first, crate::selection::SelectMode::Replace);
    world.insert_resource(selection);
    let ctx = egui::Context::default();
    let tree = open(&ctx, world, "PointLight", "color eyedropper");
    assert!(has(&tree, "color eyedropper"), "{:?}", texts(&tree));
    assert!(
        tab_to(&ctx, world, "color eyedropper"),
        "the button takes keyboard focus"
    );
    frame(&ctx, world, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
    let tree = frame(&ctx, world, Vec::new());
    assert!(world.get_resource::<ColorEyedropper>().is_some(), "armed");
    assert!(has(&tree, "Cancel the color eyedropper"), "{:?}", texts(&tree));

    // Esc cancels.
    frame(&ctx, world, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
    frame(&ctx, world, Vec::new());
    assert!(
        world.get_resource::<ColorEyedropper>().is_none(),
        "Esc disarmed it"
    );

    // Arm again, move the ring and pick with Enter.
    color_eyedropper::arm(world, vec![first], "PointLight", "color");
    frame(&ctx, world, Vec::new());
    let start = world
        .resource::<ColorEyedropper>()
        .cursor
        .expect("the ring starts at the window's center");
    frame(
        &ctx,
        world,
        vec![
            key(egui::Key::ArrowRight, egui::Modifiers::NONE),
            key(egui::Key::ArrowDown, egui::Modifiers::SHIFT),
        ],
    );
    let moved = world.resource::<ColorEyedropper>().cursor.expect("cursor");
    assert_eq!(
        moved,
        start + egui::vec2(1.0, 10.0),
        "one point right, ten down (Shift)"
    );
    let depth = world.resource::<UndoStack>().undo_depth();
    frame(&ctx, world, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
    let pending = world
        .resource::<ColorEyedropper>()
        .pending
        .clone()
        .expect("Enter picked");
    let ppp = ctx.pixels_per_point();
    assert_eq!(
        pending.pixel,
        [(moved.x * ppp).floor() as u32, (moved.y * ppp).floor() as u32]
    );
    serve_fill(world, [255, 128, 0, 255]);
    frame(&ctx, world, Vec::new());
    let expected = color_eyedropper::linear_of([255, 128, 0, 255]);
    assert_eq!(world.get::<PointLight>(first).map(|l| l.color), Some(expected));
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "one undo step"
    );
    assert!(
        world.get_resource::<ColorEyedropper>().is_none(),
        "disarmed after the pick"
    );
    let recent = picker_links::links(world, None, "").recent;
    assert_eq!(recent.first().map(|c| [c[0], c[1], c[2]]), Some(expected));
}

/// P-133, pointer path on a multi-selection: armed from the multi drawer, a click anywhere in
/// the window (the overlay takes it; nothing under it is clicked) picks for every selected light
/// as one undo step; a right click cancels.
#[test]
fn the_eyedropper_picks_with_a_click_for_every_selected_entity() {
    let mut app = app();
    let world = app.world_mut();
    let first = lamp(world, PointLight::default());
    let second = lamp(
        world,
        PointLight {
            color: [0.0, 0.0, 1.0],
            ..PointLight::default()
        },
    );
    select_all(world, &[first, second]);
    let ctx = egui::Context::default();
    let tree = open(&ctx, world, "PointLight", "color eyedropper");
    let button = find(&tree, "color eyedropper").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, button);
    let armed = world.resource::<ColorEyedropper>().clone();
    assert_eq!(armed.entities, vec![first, second], "armed for every holder");

    // A right click cancels.
    let away = egui::pos2(600.0, 1200.0);
    frame(&ctx, world, vec![egui::Event::PointerMoved(away)]);
    frame(
        &ctx,
        world,
        vec![press(away, egui::PointerButton::Secondary, true)],
    );
    frame(
        &ctx,
        world,
        vec![press(away, egui::PointerButton::Secondary, false)],
    );
    assert!(
        world.get_resource::<ColorEyedropper>().is_none(),
        "a right click cancels"
    );

    color_eyedropper::arm(world, vec![first, second], "PointLight", "color");
    frame(&ctx, world, Vec::new());
    let depth = world.resource::<UndoStack>().undo_depth();
    let at = egui::pos2(123.0, 456.0);
    click(&ctx, world, at);
    let pending = world
        .resource::<ColorEyedropper>()
        .pending
        .clone()
        .expect("the click picked");
    let ppp = ctx.pixels_per_point();
    assert_eq!(pending.pixel, [(at.x * ppp) as u32, (at.y * ppp) as u32]);
    serve_fill(world, [10, 200, 30, 255]);
    frame(&ctx, world, Vec::new());
    let expected = color_eyedropper::linear_of([10, 200, 30, 255]);
    for entity in [first, second] {
        assert_eq!(world.get::<PointLight>(entity).map(|l| l.color), Some(expected));
    }
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "one undo step"
    );
    assert!(crate::command::undo_world(world));
    assert_eq!(
        world.get::<PointLight>(second).map(|l| l.color),
        Some([0.0, 0.0, 1.0])
    );
}

/// A `[f32; 4]` color keeps its alpha; a field that is not a color is refused.
#[test]
fn the_eyedropper_keeps_alpha_and_refuses_other_fields() {
    #[derive(Reflect, Default, Debug, Clone, PartialEq, Component)]
    #[reflect(Component, Default)]
    struct Tinted {
        tint: [f32; 4],
        glow: glam::Vec4,
        size: f32,
    }
    let mut app = app();
    app.register_type::<Tinted>();
    let world = app.world_mut();
    let entity = world
        .spawn(Tinted {
            tint: [0.0, 0.0, 0.0, 0.25],
            glow: glam::Vec4::new(0.0, 0.0, 0.0, 0.5),
            size: 1.0,
        })
        .id();
    color_eyedropper::write_color(world, &[entity], "Tinted", "tint", [1.0, 0.5, 0.25]).expect("tint");
    color_eyedropper::write_color(world, &[entity], "Tinted", "glow", [0.5, 0.5, 0.5]).expect("glow");
    let tinted = world.get::<Tinted>(entity).expect("tinted").clone();
    assert_eq!(tinted.tint, [1.0, 0.5, 0.25, 0.25]);
    assert_eq!(tinted.glow, glam::Vec4::new(0.5, 0.5, 0.5, 0.5));
    let err =
        color_eyedropper::write_color(world, &[entity], "Tinted", "size", [1.0; 3]).expect_err("not a color");
    assert_eq!(err.code, "eyedropper_not_color");
}

/// P-132 in the Inspector: a camera's render mask is the layer grid (one named toggle cell per
/// render layer, keyboard operable), not a number.
#[test]
fn camera_layer_mask_draws_as_a_layer_grid() {
    let mut app = app();
    let world = app.world_mut();
    let camera = world
        .spawn((
            Name::new("Cam"),
            Transform::default(),
            Camera {
                layer_mask: 0b1,
                ..Camera::default()
            },
        ))
        .id();
    let mut selection = Selection::default();
    selection.select(camera, crate::selection::SelectMode::Replace);
    world.insert_resource(selection);
    let ctx = egui::Context::default();
    let tree = open(&ctx, world, "Camera", "layer_mask: Layer 0");
    for cell in ["layer_mask: Layer 0", "layer_mask: Layer 31"] {
        assert!(has(&tree, cell), "{cell} in {:?}", texts(&tree));
    }
    assert!(
        tab_to(&ctx, world, "layer_mask: Layer 1"),
        "a cell takes keyboard focus"
    );
    frame(&ctx, world, vec![key(egui::Key::Space, egui::Modifiers::NONE)]);
    frame(&ctx, world, Vec::new());
    assert_eq!(world.get::<Camera>(camera).map(|c| c.layer_mask), Some(0b11));
}

/// `editor.color_eyedropper`: arms and asks for the pixel, reports pending until the read-back
/// arrives, then `last` holds the color written; refusals for a non-color field and bad params.
#[test]
fn agents_pick_a_window_pixel_with_color_eyedropper() {
    let mut app = app();
    let world = app.world_mut();
    let entity = lamp(world, PointLight::default());
    let call = |world: &mut World, params: serde_json::Value| {
        let (_, handler) = crate::agent_ext::all_methods()
            .into_iter()
            .find(|(schema, _)| schema.name == "editor.color_eyedropper")
            .expect("editor.color_eyedropper is registered");
        handler(world, &params)
    };
    let reason = |err: &regia_agent::protocol::RpcError| {
        err.data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let bits = entity.to_bits();
    let answer = call(
        world,
        serde_json::json!({ "entities": [bits], "component": "PointLight", "path": "color", "pixel": [3, 4] }),
    )
    .expect("pick");
    assert_eq!(answer["armed"], true);
    assert_eq!(answer["pending"], true);
    serve_fill(world, [0, 255, 0, 255]);
    let answer = call(world, serde_json::json!({})).expect("poll");
    assert_eq!(answer["pending"], false);
    assert_eq!(answer["last"]["pixel"], serde_json::json!([3, 4]));
    assert_eq!(answer["last"]["color"], "#00ff00ff");
    assert_eq!(
        world.get::<PointLight>(entity).map(|l| l.color),
        Some([0.0, 1.0, 0.0])
    );
    let err = call(
        world,
        serde_json::json!({ "entities": [bits], "component": "PointLight", "path": "radius", "pixel": [0, 0] }),
    )
    .expect_err("radius is not a color");
    assert_eq!(reason(&err).as_deref(), Some("eyedropper_not_color"));
    let err = call(
        world,
        serde_json::json!({ "entities": [bits], "component": "PointLight", "path": "color", "pixel": [1] }),
    )
    .expect_err("bad pixel");
    assert_eq!(reason(&err).as_deref(), Some("eyedropper_bad_params"));
    color_eyedropper::arm(world, vec![entity], "PointLight", "color");
    let answer = call(world, serde_json::json!({ "cancel": true })).expect("cancel");
    assert_eq!(answer["armed"], false);
}
