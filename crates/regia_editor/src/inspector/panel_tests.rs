// SPDX-License-Identifier: MPL-2.0
//! Inspector panel tests for property search, categories, Modified only, pinned fields, the lock,
//! pinned windows and the add-component picker (spec addendum § 7.1-7.3), driven through egui
//! input (keyboard where the control has a keyboard path) and read back from the AccessKit tree.

use regia_ecs::attrs::Category;
use regia_physics::Collider;
use regia_render::{MeshSource, PointLight};
use regia_scene::prelude::{Name, Transform};

use super::pinned::{InspectorLock, InspectorPins, PinnedInspectors};
use super::search::InspectorSearch;
use super::*;

/// A component whose fields carry categories (uncategorized ones mixed in).
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct Door {
    #[reflect(@Category("Audio"))]
    creak_volume: f32,
    open_angle: f32,
    #[reflect(@Category("Motion"))]
    swing_speed: f32,
    #[reflect(@Category("Audio"))]
    slam_volume: f32,
    locked: bool,
}

fn app() -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.register_type::<MeshSource>();
    app.register_type::<PointLight>();
    app.register_type::<Collider>();
    app.register_type::<Door>();
    app.init_resource::<UndoStack>();
    app
}

fn select(world: &mut World, entity: Entity) {
    let mut selection = Selection::default();
    selection.select(entity, crate::selection::SelectMode::Replace);
    world.insert_resource(selection);
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

fn press(pos: egui::Pos2, button: egui::PointerButton, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers: egui::Modifiers::default(),
    }
}

fn click(ctx: &egui::Context, world: &mut World, pos: egui::Pos2, button: egui::PointerButton) {
    frame(
        ctx,
        world,
        vec![egui::Event::PointerMoved(pos), press(pos, button, true)],
    );
    frame(ctx, world, vec![press(pos, button, false)]);
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

/// Ctrl+F with the pointer over the Inspector, then `text` typed: the keyboard path of search.
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

fn lamp(world: &mut World, intensity: f32) -> Entity {
    world
        .spawn((
            Name::new("Lamp"),
            Transform::default(),
            PointLight {
                intensity,
                ..PointLight::default()
            },
        ))
        .id()
}

/// Item 1 acceptance: on a light, Ctrl+F then "int" shows intensity (its header expanded, the
/// other fields hidden); a non-contiguous "lgtclr" finds the light's color; Esc clears.
#[test]
fn ctrl_f_search_filters_fields_fuzzily_and_esc_clears() {
    let mut app = app();
    let entity = lamp(app.world_mut(), 900.0);
    select(app.world_mut(), entity);
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let tree = frame(&ctx, world, Vec::new());
    assert!(!has(&tree, "intensity"), "the PointLight header starts closed");
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
    // Select all and type the next query.
    frame(&ctx, world, vec![key(egui::Key::A, egui::Modifiers::COMMAND)]);
    frame(&ctx, world, vec![egui::Event::Text("lgtclr".into())]);
    let tree = frame(&ctx, world, Vec::new());
    assert_eq!(world.resource::<InspectorSearch>().query, "lgtclr");
    assert!(
        has(&tree, "color"),
        "lgtclr finds the light color: {:?}",
        texts(&tree)
    );
    assert!(!has(&tree, "intensity"));
    frame(&ctx, world, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
    let tree = frame(&ctx, world, Vec::new());
    assert_eq!(
        world.resource::<InspectorSearch>().query,
        "",
        "Esc clears the search"
    );
    assert!(
        has(&tree, "translation"),
        "every field is back: {:?}",
        texts(&tree)
    );
}

/// Item 2: @Category fields draw under collapsible sub-headers in first-seen order, after the
/// uncategorized fields.
#[test]
fn categorized_fields_draw_under_sub_headers_after_the_uncategorized_ones() {
    let mut app = app();
    let entity = app
        .world_mut()
        .spawn((Name::new("Gate"), Transform::default(), Door::default()))
        .id();
    select(app.world_mut(), entity);
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let tree = frame(&ctx, world, Vec::new());
    let header = find(&tree, "Door").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, header, egui::PointerButton::Primary);
    let tree = frame(&ctx, world, Vec::new());
    let y = |text: &str| {
        find(&tree, text)
            .unwrap_or_else(|| panic!("{text}: {:?}", texts(&tree)))
            .y
    };
    assert!(
        y("open_angle") < y("locked"),
        "uncategorized fields keep their order"
    );
    assert!(y("locked") < y("Audio"), "uncategorized fields come first");
    assert!(y("Audio") < y("creak_volume") && y("creak_volume") < y("slam_volume"));
    assert!(y("slam_volume") < y("Motion"), "categories in first-seen order");
    assert!(y("Motion") < y("swing_speed"));
    // Collapsing a category hides its fields.
    let audio = find(&tree, "Audio").expect("audio header");
    click(&ctx, world, audio, egui::PointerButton::Primary);
    let tree = frame(&ctx, world, Vec::new());
    assert!(!has(&tree, "creak_volume"), "Audio collapsed: {:?}", texts(&tree));
    assert!(has(&tree, "swing_speed"));
}

/// Item 3: Modified only hides fields equal to the component's default (the shared helper in
/// `inspector::defaults`), and components with none.
#[test]
fn modified_only_hides_fields_equal_to_their_default() {
    let mut app = app();
    let entity = lamp(app.world_mut(), PointLight::default().intensity + 50.0);
    select(app.world_mut(), entity);
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let tree = frame(&ctx, world, Vec::new());
    let toggle = find(&tree, "Modified only").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, toggle, egui::PointerButton::Primary);
    assert!(world.resource::<InspectorSearch>().modified_only);
    let tree = frame(&ctx, world, Vec::new());
    assert!(has(&tree, "intensity"), "{:?}", texts(&tree));
    assert!(
        !has(&tree, "radius"),
        "radius equals its default: {:?}",
        texts(&tree)
    );
    assert!(
        !has(&tree, "Transform"),
        "a default Transform has no modified field: {:?}",
        texts(&tree)
    );
}

/// Review sprint-v06-rev-late-k #18: Ctrl+F also works with the keyboard focus on an Inspector
/// widget and no pointer over the panel (a keyboard-only user).
#[test]
fn ctrl_f_with_the_keyboard_focus_in_the_inspector_focuses_search() {
    let mut app = app();
    let entity = lamp(app.world_mut(), 900.0);
    select(app.world_mut(), entity);
    let ctx = egui::Context::default();
    let world = app.world_mut();
    frame(&ctx, world, Vec::new());
    // Tab: the keyboard focus goes to the Inspector's first control (the lock), not the search.
    frame(&ctx, world, vec![key(egui::Key::Tab, egui::Modifiers::NONE)]);
    frame(&ctx, world, Vec::new());
    let focused = ctx.memory(|m| m.focused());
    assert!(focused.is_some(), "Tab focused an Inspector control");
    assert_ne!(
        focused,
        Some(egui::Id::new(SEARCH_BOX_ID)),
        "not the search box yet"
    );
    assert!(ctx.pointer_hover_pos().is_none(), "no pointer over the panel");
    frame(&ctx, world, vec![key(egui::Key::F, egui::Modifiers::COMMAND)]);
    frame(&ctx, world, vec![egui::Event::Text("int".to_owned())]);
    frame(&ctx, world, Vec::new());
    assert_eq!(
        world.resource::<InspectorSearch>().query,
        "int",
        "Ctrl+F moved the keyboard focus to the search box"
    );
}

/// Item 4: right-click a field ▸ Pin to top pins it for the component type; a Pinned section
/// shows it at the top for every entity with that component, and the pins file is written.
#[test]
fn pin_to_top_shows_a_pinned_section_for_every_entity_with_the_component() {
    let config = regia_test_utils::unique_temp_dir("inspector-pins-ui");
    let mut app = app();
    app.insert_resource(crate::editor_prefs::EditorConfigPaths::at(&config));
    let first = lamp(app.world_mut(), 10.0);
    let second = lamp(app.world_mut(), 20.0);
    select(app.world_mut(), first);
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let tree = frame(&ctx, world, Vec::new());
    let header = find(&tree, "PointLight").expect("header");
    click(&ctx, world, header, egui::PointerButton::Primary);
    let tree = frame(&ctx, world, Vec::new());
    assert!(!has(&tree, "Pinned"));
    let label = find(&tree, "radius").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, label, egui::PointerButton::Secondary);
    let tree = frame(&ctx, world, Vec::new());
    let pin = find(&tree, "Pin to top").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, pin, egui::PointerButton::Primary);
    assert!(
        world
            .resource::<InspectorPins>()
            .is_pinned("regia_render::lights::PointLight", "radius")
    );
    let file = std::fs::read_to_string(config.join(pinned::PINS_FILE)).expect("pins file");
    assert!(file.contains("radius"), "{file}");
    select(world, second);
    let tree = frame(&ctx, world, Vec::new());
    let tree = if has(&tree, "Pinned") {
        tree
    } else {
        let header = find(&tree, "PointLight").expect("header");
        click(&ctx, world, header, egui::PointerButton::Primary);
        frame(&ctx, world, Vec::new())
    };
    let pinned_y = find(&tree, "Pinned")
        .unwrap_or_else(|| panic!("{:?}", texts(&tree)))
        .y;
    let first_field = find(&tree, "position").expect("position").y;
    assert!(pinned_y < first_field, "the Pinned section is at the top");
    // Each row has two nodes named radius (the label and the number's spoken name).
    let radius_y: Vec<f32> = tree
        .nodes
        .iter()
        .filter(|(_, node)| node.label() == Some("radius") || node.value() == Some("radius"))
        .filter_map(|(_, node)| node.bounds().map(|b| ((b.y0 + b.y1) / 2.0) as f32))
        .collect();
    assert!(
        radius_y.iter().any(|y| *y < first_field),
        "radius in the Pinned section: {radius_y:?}"
    );
    assert!(
        radius_y.iter().any(|y| *y > first_field),
        "radius still in place: {radius_y:?}"
    );
}

/// The field label the keyboard focus is on, if the focus is on one. A field label is a frameless
/// button (3216: egui keeps plain labels out of the Tab order), so its node is a `Button` named
/// after the field; a plain `Label` node counts too.
fn focused_label(tree: &egui::accesskit::TreeUpdate) -> Option<String> {
    use egui::accesskit::Role;
    tree.nodes
        .iter()
        .find(|(id, node)| *id == tree.focus && matches!(node.role(), Role::Label | Role::Button))
        .and_then(|(_, node)| node.label().or(node.value()).map(str::to_owned))
}

/// Review sprint-v06-rev-late-k #24: Pin to top has a keyboard path. Tab to a field's label,
/// Shift+F10 opens its menu, Tab reaches Pin to top and Enter pins the field.
#[test]
fn pin_to_top_works_from_the_keyboard() {
    let config = regia_test_utils::unique_temp_dir("inspector-pins-keys");
    let mut app = app();
    app.insert_resource(crate::editor_prefs::EditorConfigPaths::at(&config));
    let entity = lamp(app.world_mut(), 10.0);
    select(app.world_mut(), entity);
    let ctx = egui::Context::default();
    let world = app.world_mut();
    // Setup only: open the PointLight header so its fields are drawn.
    let tree = frame(&ctx, world, Vec::new());
    let header = find(&tree, "PointLight").expect("header");
    click(&ctx, world, header, egui::PointerButton::Primary);
    let mut tree = frame(&ctx, world, Vec::new());
    assert!(has(&tree, "radius"), "{:?}", texts(&tree));
    // Keyboard from here on: Tab until the radius label has the focus.
    let mut tabs = 0;
    while focused_label(&tree).as_deref() != Some("radius") {
        assert!(tabs < 400, "Tab never reached the radius label");
        tree = frame(&ctx, world, vec![key(egui::Key::Tab, egui::Modifiers::NONE)]);
        tabs += 1;
    }
    let tree = frame(&ctx, world, vec![key(egui::Key::F10, egui::Modifiers::SHIFT)]);
    assert!(
        has(&tree, "Pin to top"),
        "Shift+F10 opened the field menu: {:?}",
        texts(&tree)
    );
    // Tab walks the menu's items: the field tools (Copy value, Paste value, Copy as RON, Copy agent
    // path, Reset to default; 3216), Add to sequence, then Pin to top.
    let mut tree = tree;
    let mut steps = 0;
    while tree
        .nodes
        .iter()
        .find(|(id, _)| *id == tree.focus)
        .and_then(|(_, node)| node.label().or(node.value()))
        != Some("Pin to top")
    {
        assert!(steps < 12, "Tab never reached Pin to top: {:?}", texts(&tree));
        tree = frame(&ctx, world, vec![key(egui::Key::Tab, egui::Modifiers::NONE)]);
        steps += 1;
    }
    frame(&ctx, world, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
    frame(&ctx, world, Vec::new());
    assert!(
        world
            .resource::<InspectorPins>()
            .is_pinned("regia_render::lights::PointLight", "radius"),
        "Enter on Pin to top pinned the field"
    );
    let _ = std::fs::remove_dir_all(&config);
}

/// Item 5: the lock keeps the Inspector on its entity while the selection changes, and drops
/// when the entity is despawned; Open pinned inspector opens a floating window that closes with
/// its entity.
#[test]
fn the_lock_and_pinned_windows_follow_their_entity() {
    let mut app = app();
    let lamp_entity = lamp(app.world_mut(), 1.0);
    let other = app
        .world_mut()
        .spawn((Name::new("Rock"), Transform::default()))
        .id();
    select(app.world_mut(), lamp_entity);
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let tree = frame(&ctx, world, Vec::new());
    let lock =
        find(&tree, "Lock the Inspector to this entity").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, lock, egui::PointerButton::Primary);
    assert_eq!(world.resource::<InspectorLock>().entity, Some(lamp_entity));
    select(world, other);
    let tree = frame(&ctx, world, Vec::new());
    assert!(has(&tree, "Entity: Lamp"), "locked: {:?}", texts(&tree));
    let open = find(&tree, "Open pinned inspector").expect("open pinned");
    click(&ctx, world, open, egui::PointerButton::Primary);
    assert_eq!(world.resource::<PinnedInspectors>().windows, [lamp_entity]);
    let tree = frame(&ctx, world, Vec::new());
    assert!(has(&tree, "Inspector: Lamp"), "window title: {:?}", texts(&tree));
    world.despawn(lamp_entity);
    let tree = frame(&ctx, world, Vec::new());
    assert!(has(&tree, "Entity: Rock"), "the lock dropped: {:?}", texts(&tree));
    assert!(
        !has(&tree, "Inspector: Lamp"),
        "the window closed with its entity"
    );
    assert_eq!(world.resource::<InspectorLock>().entity, None);
    assert!(world.resource::<PinnedInspectors>().windows.is_empty());
}

/// Item 6: Add component… opens a search box (focused); typing filters, Enter adds the first
/// match as one undo step; a component already present is listed grayed with the reason.
#[test]
fn the_add_component_picker_searches_and_enter_adds() {
    let mut app = app();
    let entity = lamp(app.world_mut(), 1.0);
    select(app.world_mut(), entity);
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let tree = frame(&ctx, world, Vec::new());
    let add = find(&tree, "Add component…").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, add, egui::PointerButton::Primary);
    frame(&ctx, world, vec![egui::Event::Text("pointli".into())]);
    let tree = frame(&ctx, world, Vec::new());
    assert!(has(&tree, "PointLight"), "{:?}", texts(&tree));
    assert!(
        texts(&tree).iter().any(|t| t.contains("already on this entity")),
        "the present type is grayed with its reason: {:?}",
        texts(&tree)
    );
    // Select all, type a new query, Enter adds the first available row.
    frame(&ctx, world, vec![key(egui::Key::A, egui::Modifiers::COMMAND)]);
    frame(&ctx, world, vec![egui::Event::Text("Collider".into())]);
    frame(&ctx, world, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
    frame(&ctx, world, Vec::new());
    assert!(
        world.get::<Collider>(entity).is_some(),
        "Enter added the Collider"
    );
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    let picker = world.resource::<categories::AddComponentPicker>();
    assert!(!picker.open, "the picker closes after a pick");
    assert_eq!(
        picker.recent.first().map(String::as_str),
        Some("regia_physics::components::Collider")
    );
}

/// Lead decision on review sprint-v06-rev-late-k #21: Down in the picker's search box highlights
/// the next row that can be added and Enter adds that row (not the first match).
#[test]
fn down_then_enter_adds_the_highlighted_component() {
    let mut app = app();
    let entity = lamp(app.world_mut(), 1.0);
    select(app.world_mut(), entity);
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let tree = frame(&ctx, world, Vec::new());
    let add = find(&tree, "Add component…").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, add, egui::PointerButton::Primary);
    frame(&ctx, world, Vec::new());
    // The rows the picker lists for this entity with no query.
    let present: std::collections::BTreeSet<String> = world
        .inspect_entity(entity)
        .expect("entity")
        .map(|(_, info)| info.name().to_string())
        .collect();
    let entries = {
        let registry = world.resource::<regia_ecs::reflect::AppTypeRegistry>().clone();
        let registry = registry.read();
        categories::picker_entries(&registry, &present, "", &[])
    };
    let first = categories::enter_target(&entries, None)
        .expect("an addable row")
        .type_path;
    let second = categories::step_highlight(&entries, None, true).expect("a second row");
    assert_ne!(first, second, "the test needs two addable rows: {entries:?}");
    frame(
        &ctx,
        world,
        vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)],
    );
    frame(&ctx, world, Vec::new());
    assert_eq!(
        world
            .resource::<categories::AddComponentPicker>()
            .highlight
            .as_deref(),
        Some(second),
        "Down highlighted the next addable row"
    );
    frame(&ctx, world, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
    frame(&ctx, world, Vec::new());
    let now: Vec<String> = world
        .inspect_entity(entity)
        .expect("entity")
        .map(|(_, info)| info.name().to_string())
        .collect();
    assert!(
        now.iter().any(|name| name == second),
        "Enter added the highlighted {second} (the first match is {first}): {now:?}"
    );
}

/// Item 6: an entity with many components gets a component filter box over the list.
#[test]
fn a_component_filter_narrows_a_long_component_list() {
    let mut app = app();
    let entity = app
        .world_mut()
        .spawn((
            Name::new("Busy"),
            Transform::default(),
            PointLight::default(),
            Collider::default(),
            MeshSource::new("builtin:cube", ""),
            Door::default(),
        ))
        .id();
    select(app.world_mut(), entity);
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let tree = frame(&ctx, world, Vec::new());
    let filter = find(&tree, "Filter components").unwrap_or_else(|| panic!("{:?}", texts(&tree)));
    click(&ctx, world, filter, egui::PointerButton::Primary);
    frame(&ctx, world, vec![egui::Event::Text("door".into())]);
    let tree = frame(&ctx, world, Vec::new());
    assert!(has(&tree, "Door"), "{:?}", texts(&tree));
    assert!(!has(&tree, "PointLight"), "{:?}", texts(&tree));
    assert!(!has(&tree, "Collider"));
}
