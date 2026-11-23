// SPDX-License-Identifier: MPL-2.0
//! Accessibility tree export and screen-reader hooks (UI L14, ADR-865). Headless, no GPU: the
//! tree is built from components, and the sync system is driven by plain `app.update()` calls
//! (no clock is read).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_ui::a11y::atspi::AtSpiStub;
use regia_ui::a11y::{A11yRole, A11yTree, Accessible, Politeness, ScreenReader, build_a11y_tree};
use regia_ui::prelude::*;
use regia_ui::{FocusState, Focused, LayerInputMode};

fn headless(backend: Option<AtSpiStub>) -> App {
    let mut app = App::new();
    app.insert_resource(TextSystem::embedded_only());
    if let Some(stub) = backend {
        app.insert_resource(ScreenReader::with_backend(Box::new(stub)));
    }
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app
}

struct Menu {
    root: Entity,
    start: Entity,
    toggle: Entity,
    slider: Entity,
    input: Entity,
    version: Entity,
    status: Entity,
}

/// A settings menu: root > column > (button, toggle, slider, text field, version text, status,
/// a hidden panel and a decorative star).
fn menu(world: &mut World) -> Menu {
    let root = world.spawn((UiRoot::default(), UiNode)).id();
    let column = world.spawn((UiNode, ChildOf(root))).id();
    let start = world
        .spawn((UiNode, Button, Focusable::default(), ChildOf(column)))
        .id();
    world.spawn((UiNode, Text::new("Start"), ChildOf(start)));
    let toggle = world
        .spawn((
            UiNode,
            Toggle { checked: true },
            Focusable::default(),
            ChildOf(column),
        ))
        .id();
    world.spawn((UiNode, Text::new("Subtitles"), ChildOf(toggle)));
    let slider = world
        .spawn((
            UiNode,
            Slider {
                value: 50.0,
                min: 0.0,
                max: 100.0,
                ..Slider::default()
            },
            Accessible::default().with_label("Volume"),
            ChildOf(column),
        ))
        .id();
    let mut name_field = TextInput::default();
    name_field.text = "Ada".to_owned();
    name_field.placeholder = "Player name".to_owned();
    let input = world.spawn((UiNode, name_field, ChildOf(column))).id();
    let version = world
        .spawn((UiNode, Text::new("Version 1.0"), ChildOf(column)))
        .id();
    let status = world
        .spawn((
            UiNode,
            Text::new("Ready"),
            Accessible::default()
                .with_role(A11yRole::Status)
                .with_live(Politeness::Polite),
            ChildOf(column),
        ))
        .id();
    let hidden = world
        .spawn((
            UiNode,
            NodeStyle(Style {
                display: Display::None,
                ..Style::default()
            }),
            ChildOf(column),
        ))
        .id();
    world.spawn((UiNode, Button, ChildOf(hidden)));
    world.spawn((UiNode, Text::new("*"), Accessible::decorative(), ChildOf(column)));
    Menu {
        root,
        start,
        toggle,
        slider,
        input,
        version,
        status,
    }
}

fn ids(entities: &[Entity]) -> Vec<u64> {
    entities.iter().map(|e| e.to_bits()).collect()
}

#[test]
fn the_tree_reads_like_the_screen() {
    let mut world = World::new();
    let m = menu(&mut world);
    let tree = build_a11y_tree(&mut world);
    let root = tree.get(m.root.to_bits()).expect("root");
    assert_eq!(root.role, Some(A11yRole::Window));
    // The unnamed column is flattened; the hidden panel and the decorative star are left out.
    assert_eq!(
        root.children,
        ids(&[m.start, m.toggle, m.slider, m.input, m.version, m.status])
    );
    assert_eq!(
        tree.nodes.len(),
        7,
        "root + six controls; label texts fold into their parents"
    );

    let start = tree.get(m.start.to_bits()).expect("button");
    assert_eq!(
        (start.role, start.name.as_str()),
        (Some(A11yRole::Button), "Start")
    );
    assert!(start.states.focusable && start.children.is_empty());
    assert_eq!(start.spoken_summary(), "Start, button");

    let toggle = tree.get(m.toggle.to_bits()).expect("toggle");
    assert_eq!(toggle.role, Some(A11yRole::CheckBox));
    assert_eq!(toggle.states.checked, Some(true));
    assert_eq!(toggle.spoken_summary(), "Subtitles, check box, checked");

    let slider = tree.get(m.slider.to_bits()).expect("slider");
    assert_eq!(slider.spoken_summary(), "Volume, slider, 50");

    let input = tree.get(m.input.to_bits()).expect("input");
    assert_eq!(input.role, Some(A11yRole::TextInput));
    assert_eq!(
        input.name, "Player name",
        "the placeholder names an unlabelled field"
    );
    assert_eq!(input.value.as_deref(), Some("Ada"));

    let version = tree.get(m.version.to_bits()).expect("text");
    assert_eq!(
        (version.role, version.name.as_str()),
        (Some(A11yRole::Label), "Version 1.0")
    );
    let status = tree.get(m.status.to_bits()).expect("status");
    assert_eq!(status.role, Some(A11yRole::Status));
    assert_eq!(status.live, Politeness::Polite);
    assert_eq!(tree.focus, None);
}

#[test]
fn a_secret_field_exposes_only_its_mask() {
    let mut world = World::new();
    let root = world.spawn((UiRoot::default(), UiNode)).id();
    let mut password = TextInput::default();
    password.text = "hunter2".to_owned();
    password.secret = Some('*');
    let field = world
        .spawn((
            UiNode,
            password,
            Accessible::default().with_label("Password"),
            ChildOf(root),
        ))
        .id();
    let tree = build_a11y_tree(&mut world);
    let node = tree.get(field.to_bits()).expect("field");
    assert_eq!(node.value.as_deref(), Some("*******"));
    assert!(!node.spoken_summary().contains("hunter2"));
}

#[test]
fn focus_and_layers_are_reported() {
    let mut world = World::new();
    let m = menu(&mut world);
    // A dialog layer over the menu: the menu stays in the tree but is inert.
    let dialog_root = world.spawn((UiRoot::default(), UiNode)).id();
    let dialog = world
        .spawn((
            UiNode,
            Modal {
                open: true,
                ..Modal::default()
            },
            ChildOf(dialog_root),
        ))
        .id();
    world.spawn((UiNode, Text::new("Quit the game?"), ChildOf(dialog)));
    world.init_resource::<regia_ui::InputLayers>();
    regia_ui::layers::push_layer(&mut world, dialog, LayerInputMode::UiOnly).expect("push");
    // Focus set after the push (the push resolves the layer's own default focus).
    world.entity_mut(m.toggle).insert(Focused);
    world.resource_mut::<FocusState>().focused = Some(m.toggle);

    let tree = build_a11y_tree(&mut world);
    assert_eq!(tree.focus, Some(m.toggle.to_bits()));
    let toggle = tree.get(m.toggle.to_bits()).expect("toggle");
    assert!(toggle.states.focused && toggle.states.inert);
    let dialog_node = tree.get(dialog.to_bits()).expect("dialog");
    assert_eq!(dialog_node.role, Some(A11yRole::Dialog));
    assert_eq!(dialog_node.name, "Quit the game?");
    assert_eq!(dialog_node.states.expanded, Some(true));
    assert!(!dialog_node.states.inert);
}

fn reader(app: &App) -> &ScreenReader {
    app.world().resource::<ScreenReader>()
}

#[test]
fn an_inactive_reader_builds_nothing() {
    let mut app = headless(Some(AtSpiStub::new(false)));
    menu(app.world_mut());
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(reader(&app).builds, 0);
    assert!(reader(&app).tree().nodes.is_empty());
    assert!(reader(&app).backend().recent_events().is_empty());
    // Announcements are still remembered for the agent.
    app.world_mut()
        .resource_mut::<ScreenReader>()
        .announce("Paused", Politeness::Assertive);
    app.update();
    let recent: Vec<(String, Politeness)> = reader(&app)
        .recent_announcements()
        .map(|(t, p)| (t.to_owned(), p))
        .collect();
    assert_eq!(recent, vec![("Paused".to_owned(), Politeness::Assertive)]);
}

#[test]
fn a_live_reader_gets_the_tree_then_only_changes() {
    let mut app = headless(Some(AtSpiStub::new(true)));
    let m = menu(app.world_mut());
    app.update();
    assert_eq!(reader(&app).builds, 1);
    let first = reader(&app).backend().recent_events();
    let adds = first
        .iter()
        .filter(|s| s.starts_with("object:children-changed:add"))
        .count();
    assert_eq!(adds, 7, "every node is reported once: {first:?}");

    // Nothing changed: the stored tree is kept and nothing reaches the backend.
    app.update();
    assert_eq!(reader(&app).builds, 1);
    assert_eq!(reader(&app).backend().recent_events().len(), first.len());

    // A state change, a value change and a live-region text change.
    app.world_mut()
        .get_mut::<Toggle>(m.toggle)
        .expect("toggle")
        .checked = false;
    app.world_mut().get_mut::<Slider>(m.slider).expect("slider").value = 75.0;
    app.world_mut().get_mut::<Text>(m.status).expect("status").spans[0].content = "Saved".into();
    app.update();
    let all = reader(&app).backend().recent_events();
    let new: Vec<&String> = all[first.len()..].iter().collect();
    let path = |e: Entity| format!("/org/a11y/atspi/accessible/{}", e.to_bits());
    assert!(
        new.iter()
            .any(|s| s.starts_with(&format!("object:state-changed:checked {} 0", path(m.toggle)))),
        "{new:?}"
    );
    assert!(
        new.iter().any(|s| s.starts_with(&format!(
            "object:property-change:accessible-value {} 0 75",
            path(m.slider)
        ))),
        "{new:?}"
    );
    assert!(
        new.iter()
            .any(|s| s.starts_with("object:announcement") && s.ends_with(" 1 Saved")),
        "{new:?}"
    );
    let recent: Vec<&str> = reader(&app).recent_announcements().map(|(t, _)| t).collect();
    assert_eq!(recent, vec!["Saved"]);

    // Removing a node reports it.
    app.world_mut().entity_mut(m.version).despawn();
    app.update();
    let all = reader(&app).backend().recent_events();
    assert!(
        all.last()
            .is_some_and(|s| s.starts_with("object:children-changed:remove") && s.ends_with(&path(m.version))),
        "{all:?}"
    );
}

#[test]
fn turning_exporting_off_and_on_reports_the_whole_tree_again() {
    let mut app = headless(Some(AtSpiStub::new(false)));
    menu(app.world_mut());
    app.world_mut().resource_mut::<ScreenReader>().set_exporting(true);
    app.update();
    assert_eq!(reader(&app).tree().nodes.len(), 7);
    app.world_mut()
        .resource_mut::<ScreenReader>()
        .set_exporting(false);
    app.update();
    assert!(
        reader(&app).tree().nodes.is_empty(),
        "the tree is dropped while not live"
    );
    app.world_mut().resource_mut::<ScreenReader>().set_exporting(true);
    app.update();
    assert_eq!(reader(&app).tree().nodes.len(), 7);
    assert_eq!(reader(&app).builds, 2);
}

#[test]
fn the_default_backend_is_never_active_without_opt_in() {
    // REGIA_SCREEN_READER is not set by the test harness.
    if regia_core::brand::env_var_os("REGIA_SCREEN_READER").is_some() {
        return;
    }
    let reader = ScreenReader::default();
    assert!(!reader.is_live());
    let tree: &A11yTree = reader.tree();
    assert!(tree.nodes.is_empty());
}
