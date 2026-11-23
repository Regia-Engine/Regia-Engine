// SPDX-License-Identifier: MPL-2.0
//! Inspector field-tool tests (spec addendum § 7.4-7.6): copy/paste and reset from the keyboard,
//! `@EditCondition`, the raw view, `@Toggle` / `Option` header checkboxes and the list tools,
//! driven through egui input (Tab focus, keys) and read back from the AccessKit tree.

use regia_ecs::attrs::{EditCondition, EditConditionHides, Hidden, Toggle};

use regia_scene::prelude::{Name, Transform};

use super::clipboard::{InspectorClipboard, InspectorRawView};
use super::*;

/// A `@Toggle` group.
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
struct Fog {
    enabled: bool,
    density: f32,
}

/// One component with every field tool's case.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
struct Gadget {
    /// How strong the gadget is.
    power: f32,
    shadows: bool,
    #[reflect(@EditCondition("shadows"))]
    softness: f32,
    #[reflect(@EditCondition("shadows"), @EditConditionHides)]
    bias: f32,
    #[reflect(@Hidden)]
    secret: u32,
    #[reflect(@Toggle)]
    fog: Fog,
    tint: Option<f32>,
    steps: Vec<f32>,
}

impl Default for Gadget {
    fn default() -> Self {
        Self {
            power: 1.0,
            shadows: false,
            softness: 0.0,
            bias: 0.0,
            secret: 0,
            fog: Fog::default(),
            tint: None,
            steps: Vec::new(),
        }
    }
}

fn app() -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<Gadget>();
    app.register_type::<Fog>();
    app.register_type::<Option<f32>>();
    app.register_type::<Vec<f32>>();
    app.init_resource::<UndoStack>();
    app
}

fn gadget(world: &mut World, gadget: Gadget) -> Entity {
    world
        .spawn((Name::new("Thing"), Transform::default(), gadget))
        .id()
}

fn select(world: &mut World, entity: Entity) {
    let mut selection = Selection::default();
    selection.select(entity, crate::selection::SelectMode::Replace);
    world.insert_resource(selection);
}

fn frame(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>) -> egui::accesskit::TreeUpdate {
    ctx.enable_accesskit();
    ctx.all_styles_mut(|style| style.animation_time = 0.0);
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(900.0, 2400.0),
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

fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
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

/// Presses Tab until the widget labeled `label` has keyboard focus (the keyboard path).
fn tab_to(ctx: &egui::Context, world: &mut World, label: &str) {
    let mut seen = Vec::new();
    for _ in 0..400 {
        let tree = frame(ctx, world, vec![key(egui::Key::Tab, egui::Modifiers::NONE)]);
        let focused = tree
            .nodes
            .iter()
            .find(|(id, _)| *id == tree.focus)
            .and_then(|(_, node)| node.label().map(str::to_owned));
        if focused.as_deref() == Some(label) {
            return;
        }
        if let Some(focused) = focused {
            seen.push(focused);
        }
    }
    panic!("Tab never reached {label:?}; focus went through {seen:?}");
}

/// Tab to `label` and press `key` on it.
fn press_on(ctx: &egui::Context, world: &mut World, label: &str, pressed: egui::Key) {
    tab_to(ctx, world, label);
    frame(ctx, world, vec![key(pressed, egui::Modifiers::NONE)]);
    frame(ctx, world, Vec::new());
}

/// Opens the Gadget header from the keyboard.
fn open_gadget(ctx: &egui::Context, world: &mut World) {
    press_on(ctx, world, "Gadget", egui::Key::Enter);
}

fn gadget_of(world: &World, entity: Entity) -> Gadget {
    world.get::<Gadget>(entity).cloned().expect("gadget")
}

/// P-115 + P-114 acceptance: Tab to a field's label, Ctrl+C copies its value; on another entity
/// Ctrl+V pastes it (one undo step); the field's reset arrow (Tab + Enter) puts the default back.
#[test]
fn copy_paste_and_reset_from_the_keyboard() {
    let mut app = app();
    let a = gadget(
        app.world_mut(),
        Gadget {
            power: 5.0,
            ..Gadget::default()
        },
    );
    let b = gadget(
        app.world_mut(),
        Gadget {
            power: 9.0,
            ..Gadget::default()
        },
    );
    let ctx = egui::Context::default();
    let world = app.world_mut();
    select(world, a);
    open_gadget(&ctx, world);
    tab_to(&ctx, world, "power");
    frame(&ctx, world, vec![egui::Event::Copy]);
    let clip = world
        .get_resource::<InspectorClipboard>()
        .and_then(|clip| clip.entry.as_ref())
        .map(|entry| (entry.type_path.clone(), entry.ron.clone()));
    assert_eq!(
        clip,
        Some(("f32".to_owned(), "5.0".to_owned())),
        "Ctrl+C copied power"
    );

    select(world, b);
    frame(&ctx, world, Vec::new());
    let depth = world.resource::<UndoStack>().undo_depth();
    tab_to(&ctx, world, "power");
    frame(&ctx, world, vec![egui::Event::Paste(String::new())]);
    assert_eq!(gadget_of(world, b).power, 5.0, "Ctrl+V pasted");
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "one undo step"
    );

    // A bool cannot take the f32: refused, the value stays.
    tab_to(&ctx, world, "shadows");
    frame(&ctx, world, vec![egui::Event::Paste(String::new())]);
    assert!(!gadget_of(world, b).shadows);
    let notice = world
        .get_resource::<InspectorClipboard>()
        .and_then(|clip| clip.notice.clone())
        .expect("a notice");
    assert!(notice.error && notice.text.contains("f32"), "{notice:?}");

    // power (5) differs from its default (1): the reset arrow is there and resets it.
    press_on(&ctx, world, "Reset power to default", egui::Key::Enter);
    assert_eq!(gadget_of(world, b).power, 1.0);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth + 2);
    let tree = frame(&ctx, world, Vec::new());
    assert!(
        !has(&tree, "Reset power to default"),
        "no arrow on a default value"
    );

    // The tooltip names the doc, the default and the agent path.
    let value = gadget_of(world, b);
    let tools = super::clipboard::FieldTools::for_component(world, b, "Gadget", &value);
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let mut context = InspectorContext::new("Gadget").with_field_tools(tools);
    let drawer = InspectorUi::new(&registry, &mut context);
    let lines = drawer
        .tooltip_for("power", None, Some("How strong the gadget is."))
        .lines();
    assert_eq!(lines[0], "How strong the gadget is.");
    assert_eq!(lines[1], "Default: 1.0");
    assert!(lines[2].starts_with("Agent: editor.set_property"), "{lines:?}");
}

/// P-119 + P-120: a false `@EditCondition` greys its field (or hides it with
/// `@EditConditionHides`); the raw view shows `@Hidden` fields read-only and marked.
#[test]
fn edit_conditions_and_the_raw_view() {
    let mut app = app();
    let e = gadget(app.world_mut(), Gadget::default());
    let ctx = egui::Context::default();
    let world = app.world_mut();
    select(world, e);
    open_gadget(&ctx, world);
    let tree = frame(&ctx, world, Vec::new());
    assert!(has(&tree, "softness"), "greyed, still shown");
    assert!(!has(&tree, "bias"), "hidden while shadows is off");
    assert!(!has(&tree, "secret (hidden)"));
    // softness is read-only while shadows is off: its value widget is disabled.
    let disabled = tree.nodes.iter().filter(|(_, node)| node.is_disabled()).count();
    assert!(disabled > 0, "greyed widgets are disabled in the tree");

    world.get_mut::<Gadget>(e).expect("gadget").shadows = true;
    let tree = frame(&ctx, world, Vec::new());
    assert!(has(&tree, "bias"), "shown once shadows is on");

    world.insert_resource(InspectorRawView(true));
    let tree = frame(&ctx, world, Vec::new());
    assert!(
        has(&tree, "secret (hidden)"),
        "raw shows the hidden field: {:?}",
        texts(&tree)
    );
    assert!(has(&tree, "Raw"), "the header toggle");
}

/// P-121: a `@Toggle` group's `enabled` and an `Option` field are header checkboxes (Tab +
/// Space), one undo step each; an off group's body is greyed.
#[test]
fn toggle_and_option_checkboxes_from_the_keyboard() {
    let mut app = app();
    let e = gadget(app.world_mut(), Gadget::default());
    let ctx = egui::Context::default();
    let world = app.world_mut();
    select(world, e);
    open_gadget(&ctx, world);
    let depth = world.resource::<UndoStack>().undo_depth();
    press_on(&ctx, world, "Enable fog", egui::Key::Space);
    assert!(gadget_of(world, e).fog.enabled);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth + 1);
    let tree = frame(&ctx, world, Vec::new());
    assert!(
        !has(&tree, "enabled"),
        "the group's enabled row is its header checkbox"
    );
    press_on(&ctx, world, "Enable tint", egui::Key::Space);
    assert_eq!(gadget_of(world, e).tint, Some(0.0));
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth + 2);
    assert!(crate::command::undo_world(world));
    assert_eq!(gadget_of(world, e).tint, None);
}

/// P-122: the list shows its count; a focused drag handle moves its element with Alt+Up;
/// Duplicate copies an element; Clear asks first. One undo step each.
#[test]
fn list_count_reorder_duplicate_and_clear_from_the_keyboard() {
    let mut app = app();
    let e = gadget(
        app.world_mut(),
        Gadget {
            steps: vec![1.0, 2.0, 3.0],
            ..Gadget::default()
        },
    );
    let ctx = egui::Context::default();
    let world = app.world_mut();
    select(world, e);
    open_gadget(&ctx, world);
    let tree = frame(&ctx, world, Vec::new());
    assert!(has(&tree, "3 items"), "{:?}", texts(&tree));
    let depth = world.resource::<UndoStack>().undo_depth();
    tab_to(&ctx, world, "Drag to reorder element 2 (Alt+Up or Alt+Down)");
    frame(&ctx, world, vec![key(egui::Key::ArrowUp, egui::Modifiers::ALT)]);
    assert_eq!(gadget_of(world, e).steps, vec![1.0, 3.0, 2.0]);
    press_on(&ctx, world, "Duplicate element 0", egui::Key::Enter);
    assert_eq!(gadget_of(world, e).steps, vec![1.0, 1.0, 3.0, 2.0]);
    press_on(&ctx, world, "Clear", egui::Key::Enter);
    assert_eq!(gadget_of(world, e).steps.len(), 4, "Clear asks first");
    press_on(&ctx, world, "Clear all", egui::Key::Enter);
    assert!(gadget_of(world, e).steps.is_empty());
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth + 3);
}
