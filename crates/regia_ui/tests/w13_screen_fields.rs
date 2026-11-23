// SPDX-License-Identifier: MPL-2.0
//! w13 acceptance: a saved project scale rule, screen back, subtree disable and node pivot
//! (ADR-8252). Headless: no window, no GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_ui::focus::{DefaultFocus, Focusable};
use regia_ui::layers::LayerInputMode;
use regia_ui::prelude::*;
use regia_ui::screens::{ScreenStack, push_screen};
use regia_ui::style::{Dimension, FlexDirection, Style};
use regia_ui::{NodeName, UiTransform};

const STEP_MS: u64 = 100;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(STEP_MS)));
    app
}

fn settings_app(text: &str) -> (App, std::path::PathBuf) {
    use regia_platform::{DirMount, MountPoint, Vfs};
    let dir = regia_test_utils::unique_temp_dir("w13-ui-settings");
    std::fs::create_dir_all(dir.join("ui")).expect("ui dir");
    std::fs::write(dir.join("ui/project.uisettings.ron"), text).expect("file");
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::Assets, DirMount::read_only(dir.clone()));
    let mut app = app();
    app.insert_resource(vfs);
    (app, dir)
}

fn depth(app: &App) -> usize {
    app.world().resource::<ScreenStack>().depth()
}

fn by_name(app: &mut App, name: &str) -> Entity {
    let world = app.world_mut();
    let mut names = world.query::<(Entity, &NodeName)>();
    names
        .iter(world)
        .find_map(|(entity, node)| (node.0 == name).then_some(entity))
        .unwrap_or_else(|| panic!("no node named {name}"))
}

/// P-256: `.uisettings.ron` v2 `scale_mode` drives a root that is still on the engine default.
/// A root the game already set keeps its mode. Integer scale of 320×180 on a 1280×720 window is 4.
#[test]
fn the_saved_scale_rule_drives_the_runtime_scale() {
    let source = "(version: 2, scale_mode: IntegerScale(reference: (320.0, 180.0)))";
    let file = regia_ui::parse_ui_settings_ron(source, "project.uisettings.ron")
        .expect("the saved scale rule parses");
    assert_eq!(file.version, regia_ui::UI_SETTINGS_VERSION);
    let (mut app, dir) = settings_app(source);
    regia_ui::scale::load_ui_settings(app.world_mut());
    let saved = app
        .world_mut()
        .spawn((UiRoot::default(), NodeStyle::new(Style::default()), UiNode))
        .id();
    let coded = app
        .world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style::default()),
            UiNode,
        ))
        .id();
    app.update();
    let later = app
        .world_mut()
        .spawn((UiRoot::default(), NodeStyle::new(Style::default()), UiNode))
        .id();
    app.update();
    let world = app.world();
    assert_eq!(
        world.get::<UiRoot>(saved).map(|r| r.scale_mode),
        Some(ScaleMode::IntegerScale {
            reference: [320.0, 180.0]
        }),
        "a default root takes the saved rule"
    );
    assert_eq!(
        world.get::<ComputedNode>(saved).map(|n| n.scale),
        Some(4.0),
        "1280×720 at a 320×180 integer rule is scale 4"
    );
    assert_eq!(
        world.get::<UiRoot>(later).map(|r| r.scale_mode),
        Some(ScaleMode::IntegerScale {
            reference: [320.0, 180.0]
        }),
        "a root spawned later still takes the saved rule"
    );
    assert_eq!(
        world.get::<UiRoot>(coded).map(|r| r.scale_mode),
        Some(ScaleMode::ConstantPixelSize),
        "a mode the game set wins over the file"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// A v1 settings file stays readable, keeps its curve and budget, and is at the current version.
#[test]
fn a_v1_uisettings_upgrades() {
    let v1 = "(version: 1, dpi_curve: [(96.0, 1.0), (192.0, 2.0)], layer_budget_bytes: 1024)";
    let sources = regia_ui::ui_settings_migration_sources();
    assert!(
        sources.contains(&1),
        "v1 must migrate to the current settings version, sources {sources:?}"
    );
    let file = regia_ui::parse_ui_settings_ron(v1, "old.uisettings.ron").expect("v1 still loads");
    assert_eq!(file.version, 2, "a v1 file is read as the current document");
    assert_eq!(file.version, regia_ui::UI_SETTINGS_VERSION);
    assert_eq!(file.dpi_curve, Some(vec![[96.0, 1.0], [192.0, 2.0]]));
    assert_eq!(file.layer_budget_bytes, Some(1024));
    assert!(file.scale_mode.is_none(), "v1 has no saved rule");
    let migrated = regia_ui::migrate_ui_settings_ron(v1).expect("v1 migrates");
    assert!(
        migrated.contains("version: 2"),
        "the migrator rewrites the version: {migrated}"
    );
    assert_eq!(
        migrated.replacen("version: 2", "version: 1", 1),
        v1,
        "every other byte stays"
    );
    let again = regia_ui::migrate_ui_settings_ron(&migrated).expect("current is unchanged");
    assert_eq!(again, migrated);
}

/// P-266: a document with `back: pop` pops on the back action. Absent and `ignore` do not.
#[test]
fn a_screen_with_back_handled_pops_on_back() {
    let pop = r#"(
        version: 11,
        back: pop,
        root: (children: [(name: "close", widget: button)]),
    )"#;
    let ignore = r#"(
        version: 11,
        back: ignore,
        root: (children: [(name: "stay", widget: button)]),
    )"#;
    let plain = r#"(
        version: 11,
        root: (children: [(name: "plain", widget: button)]),
    )"#;
    let pop_file =
        regia_ui::parse_ui_ron(pop, "pop.ui.ron", &regia_ui::UiMigrators::default()).expect("back: pop");
    let ignore_file = regia_ui::parse_ui_ron(ignore, "ignore.ui.ron", &regia_ui::UiMigrators::default())
        .expect("back: ignore");
    let plain_file = regia_ui::parse_ui_ron(plain, "plain.ui.ron", &regia_ui::UiMigrators::default())
        .expect("no back field");
    let mut app = app();
    regia_ui::authoring::insert_ui_template(app.world_mut(), "pop", "ui/pop.ui.ron", pop_file);
    regia_ui::authoring::insert_ui_template(app.world_mut(), "ignore", "ui/ignore.ui.ron", ignore_file);
    regia_ui::authoring::insert_ui_template(app.world_mut(), "plain", "ui/plain.ui.ron", plain_file);

    let main = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
            UiScreen::new("main").with_mode(LayerInputMode::UiOnly),
        ))
        .id();
    let main_button = app
        .world_mut()
        .spawn((UiNode, Focusable::default(), DefaultFocus, ChildOf(main)))
        .id();
    push_screen(app.world_mut(), main).expect("push main");
    app.update();

    let spawn_top = |app: &mut App, name: &str, id: &str| {
        let root = regia_ui::authoring::spawn_ui(app.world_mut(), name).expect("spawn");
        app.world_mut()
            .entity_mut(root)
            .insert(UiScreen::new(id).with_mode(LayerInputMode::UiOnly));
        push_screen(app.world_mut(), root).expect("push");
        app.update();
        root
    };
    let cancel = |app: &mut App| {
        app.world_mut()
            .resource_mut::<regia_ui::focus::UiNavActions>()
            .cancel = true;
        app.update();
    };

    spawn_top(&mut app, "pop", "settings");
    assert_eq!(depth(&app), 2);
    cancel(&mut app);
    assert_eq!(depth(&app), 1, "back: pop pops the screen");
    assert_eq!(
        app.world().resource::<regia_ui::focus::FocusState>().focused,
        Some(main_button),
        "pop restores the control that had focus"
    );

    spawn_top(&mut app, "ignore", "ignored");
    cancel(&mut app);
    assert_eq!(depth(&app), 2, "back: ignore leaves the screen up");

    spawn_top(&mut app, "plain", "plain");
    cancel(&mut app);
    assert_eq!(depth(&app), 3, "a document with no back field does not pop");
}

/// P-273: `disabled_subtree` on a node makes that node and its descendants ignore pointer input.
/// A sibling outside the subtree still receives the press.
#[test]
fn a_disabled_subtree_ignores_input() {
    let source = r#"(
        version: 11,
        root: (children: [
            (name: "group", disabled_subtree: true, children: [
                (name: "inner", interactable: true),
            ]),
            (name: "outside", interactable: true),
        ]),
    )"#;
    let file = regia_ui::parse_ui_ron(source, "tree.ui.ron", &regia_ui::UiMigrators::default())
        .expect("disabled_subtree");
    let mut app = app();
    let parent = app.world_mut().spawn(UiNode).id();
    for child in &file.root.children {
        regia_ui::authoring::spawn_node(app.world_mut(), parent, child);
    }
    let inner = by_name(&mut app, "inner");
    let outside = by_name(&mut app, "outside");
    let place = |app: &mut App, entity: Entity, rect: [f32; 4]| {
        app.world_mut().entity_mut(entity).insert(ComputedNode {
            rect,
            clip_rect: rect,
            ..Default::default()
        });
    };
    place(&mut app, inner, [0.0, 0.0, 40.0, 40.0]);
    place(&mut app, outside, [80.0, 0.0, 40.0, 40.0]);
    app.update();
    assert!(
        regia_ui::node::is_disabled(app.world(), inner),
        "the child of a disabled subtree is disabled"
    );
    assert!(
        !regia_ui::node::is_disabled(app.world(), outside),
        "a sibling outside the subtree stays enabled"
    );
    {
        let mut pointer = app.world_mut().resource_mut::<regia_ui::interaction::UiPointer>();
        pointer.cursor = Some([10.0, 10.0]);
        pointer.primary_down = true;
    }
    app.update();
    assert_ne!(
        app.world().resource::<regia_ui::interaction::UiPointer>().pressed,
        Some(inner),
        "a press inside the disabled subtree is ignored"
    );
    {
        let mut pointer = app.world_mut().resource_mut::<regia_ui::interaction::UiPointer>();
        pointer.primary_down = false;
        pointer.pressed = None;
    }
    app.update();
    {
        let mut pointer = app.world_mut().resource_mut::<regia_ui::interaction::UiPointer>();
        pointer.cursor = Some([90.0, 10.0]);
        pointer.primary_down = true;
        pointer.pressed = None;
    }
    app.update();
    assert_eq!(
        app.world().resource::<regia_ui::interaction::UiPointer>().pressed,
        Some(outside),
        "a sibling outside the subtree still takes the press"
    );
}

/// P-253: `pivot` on a node is that node's transform pivot.
#[test]
fn a_node_pivot_is_the_transform_pivot() {
    let source = r#"(
        version: 11,
        root: (children: [(name: "icon", pivot: (0.0, 1.0))]),
    )"#;
    let file =
        regia_ui::parse_ui_ron(source, "pivot.ui.ron", &regia_ui::UiMigrators::default()).expect("pivot");
    let mut app = app();
    let parent = app.world_mut().spawn(UiNode).id();
    regia_ui::authoring::spawn_node(app.world_mut(), parent, &file.root.children[0]);
    let icon = by_name(&mut app, "icon");
    assert_eq!(
        app.world().get::<UiTransform>(icon).map(|t| t.pivot_ratio),
        Some([0.0, 1.0])
    );
}
