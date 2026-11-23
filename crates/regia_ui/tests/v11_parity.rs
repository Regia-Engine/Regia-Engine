// SPDX-License-Identifier: MPL-2.0
//! The `.ui.ron` v11 containers, markup and focus-neighbors fold (ADR-7827, addendum § 13.5): the
//! `split`, `foldable` and `tabs` containers, text `markup`, node `nav` neighbors by name, and the
//! `focus_visible` theme state (keyboard or gamepad focus only). Headless: fixed 16 ms steps, no GPU,
//! no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, MouseButton};
use regia_ui::authoring::{SplitAxisDef, UiLibrary, WidgetDef, spawn_ui};
use regia_ui::focus::FocusState;
use regia_ui::prelude::*;
use regia_ui::text::TextContent;
use regia_ui::theme::{StyleClass, VisualOverrides};
use regia_ui::{
    Focusable, Focused, FoldBody, FoldHeader, Foldable, NodeName, Split, SplitDivider, TabBar, TabPage,
    TabPages, ThemeOverride, UiMigrators, parse_ui_ron, upgrade_ui_ron, write_ui_ron,
};

const PARITY: &str = include_str!("fixtures/ui.v11-parity.example.ui.ron");
const FROZEN_V11: &str = include_str!("../../regia_agent/tests/fixtures/format-corpus/ui/v11.ui.ron");
const FROZEN_V10: &str = include_str!("../../regia_agent/tests/fixtures/format-corpus/ui/v10.ron");

fn ui_app() -> App {
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
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app
}

fn settle(app: &mut App) {
    for _ in 0..6 {
        app.update();
    }
}

fn key(app: &mut App, logical: &str) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(regia_platform::KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: logical.into(),
            logical_key: logical.into(),
            state,
            repeat: false,
            timestamp: Default::default(),
        });
        app.update();
    }
}

fn click(app: &mut App, e: Entity) {
    let c = *app.world().get::<ComputedNode>(e).unwrap();
    // Cursor positions and layout rects are both physical px.
    let at = [c.rect[0] + c.rect[2] * 0.5, c.rect[1] + c.rect[3] * 0.5];
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (f64::from(at[0]), f64::from(at[1])),
        timestamp: Default::default(),
    });
    app.update();
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(regia_platform::MouseButtonInput {
            window: Entity::PLACEHOLDER,
            button: MouseButton::Left,
            state,
            timestamp: Default::default(),
        });
        app.update();
    }
}

fn spawn_parity(app: &mut App) -> Entity {
    let file = parse_ui_ron(PARITY, "parity.ui.ron", &UiMigrators::default()).unwrap();
    app.world_mut()
        .resource_mut::<UiLibrary>()
        .templates
        .insert("parity".to_owned(), file);
    let root = spawn_ui(app.world_mut(), "parity").unwrap();
    settle(app);
    root
}

fn named(app: &mut App, name: &str) -> Entity {
    let world = app.world_mut();
    let mut q = world.query::<(Entity, &NodeName)>();
    q.iter(world)
        .find(|(_, n)| n.0 == name)
        .map(|(e, _)| e)
        .unwrap_or_else(|| panic!("no node {name}"))
}

fn display(app: &App, e: Entity) -> Display {
    app.world().get::<NodeStyle>(e).unwrap().0.display
}

fn children(app: &App, e: Entity) -> Vec<Entity> {
    app.world()
        .get::<Children>(e)
        .map(|c| c.iter().collect())
        .unwrap_or_default()
}

#[test]
fn the_parity_fixture_parses_and_writes_back_to_the_same_document() {
    let file = parse_ui_ron(PARITY, "parity.ui.ron", &UiMigrators::default()).unwrap();
    assert_eq!(file.version, 11, "the fold keeps v11 (ADR-7724)");
    let body = &file.root.children[1];
    let Some(WidgetDef::Split(split)) = &body.widget else {
        panic!("body is a split: {:?}", body.widget);
    };
    assert_eq!(split.axis, SplitAxisDef::Horizontal);
    assert_eq!((split.ratio, split.min_ratio, split.max_ratio), (0.3, 0.2, 0.8));
    assert!(file.root.children[0].text.as_ref().unwrap().markup);
    let apply = &file.root.children[2].children[0];
    assert_eq!(apply.nav.right.as_deref(), Some("reset"));
    assert_eq!(apply.nav.down.as_deref(), Some("quit"));

    let written = write_ui_ron(&file).unwrap();
    let again = parse_ui_ron(&written, "again.ui.ron", &UiMigrators::default()).unwrap();
    assert_eq!(
        again, file,
        "write then parse gives the same document:\n{written}"
    );
    assert_eq!(write_ui_ron(&again).unwrap(), written, "the writer is stable");
    for needle in ["split((", "foldable((", "tabs((", "markup: true", "nav: ("] {
        assert!(written.contains(needle), "{needle} written:\n{written}");
    }
    assert!(
        !written.contains("open: false") && !written.contains("active: 0"),
        "defaults are left out:\n{written}"
    );
}

#[test]
fn earlier_v11_and_v10_files_read_with_the_same_meaning() {
    let v11 = parse_ui_ron(FROZEN_V11, "v11.ui.ron", &UiMigrators::default()).unwrap();
    let mut stack = vec![&v11.root];
    while let Some(node) = stack.pop() {
        assert!(node.nav.is_empty(), "no neighbors in a pre-fold file");
        assert!(
            node.text.as_ref().is_none_or(|t| !t.markup),
            "no markup in a pre-fold file"
        );
        assert!(
            !matches!(
                node.widget,
                Some(WidgetDef::Split(_) | WidgetDef::Foldable(_) | WidgetDef::Tabs(_))
            ),
            "no containers in a pre-fold file"
        );
        stack.extend(node.children.iter());
    }
    let written = write_ui_ron(&v11).unwrap();
    for absent in ["nav:", "markup", "split(", "foldable(", "tabs("] {
        assert!(
            !written.contains(absent),
            "{absent} not written for a pre-fold file:\n{written}"
        );
    }
    assert_eq!(
        parse_ui_ron(&written, "v11.ui.ron", &UiMigrators::default()).unwrap(),
        v11
    );

    let upgraded = upgrade_ui_ron(FROZEN_V10, "v10.ron").unwrap();
    let v10 = parse_ui_ron(&upgraded, "v10.ron", &UiMigrators::default()).unwrap();
    assert_eq!(v10.version, 11);
    assert_eq!(
        upgrade_ui_ron(&upgraded, "v10.ron").unwrap(),
        upgraded,
        "upgrading twice = once"
    );
}

#[test]
fn containers_spawn_with_their_runtime_parts() {
    let mut app = ui_app();
    spawn_parity(&mut app);

    // Split: pane, divider, pane; the panes share the room 0.3 : 0.7.
    let body = named(&mut app, "body");
    let split = *app.world().get::<Split>(body).unwrap();
    assert_eq!((split.ratio, split.min_ratio, split.max_ratio), (0.3, 0.2, 0.8));
    let parts = children(&app, body);
    assert_eq!(parts.len(), 3, "first pane, divider, second pane");
    assert!(app.world().get::<SplitDivider>(parts[1]).is_some());
    let sections = named(&mut app, "sections");
    let pages = named(&mut app, "pages");
    assert_eq!((parts[0], parts[2]), (sections, pages));
    let w = |app: &App, e: Entity| app.world().get::<ComputedNode>(e).unwrap().rect[2];
    let (a, b) = (w(&app, sections), w(&app, pages));
    assert!((a / (a + b) - 0.3).abs() < 0.02, "pane widths {a} : {b}");

    // Foldable accordion: a header first, then the body; the closed one hides its body.
    let audio = named(&mut app, "audio_fold");
    let video = named(&mut app, "video_fold");
    assert!(app.world().get::<Foldable>(audio).unwrap().open);
    assert!(!app.world().get::<Foldable>(video).unwrap().open);
    let audio_parts = children(&app, audio);
    assert!(
        app.world().get::<FoldHeader>(audio_parts[0]).is_some(),
        "header first"
    );
    let volume = named(&mut app, "volume");
    let vsync = named(&mut app, "vsync");
    assert!(app.world().get::<FoldBody>(volume).is_some());
    assert_ne!(display(&app, volume), Display::None);
    assert_eq!(display(&app, vsync), Display::None);
    // Opening the video section through its header closes the audio one (one group).
    let video_header = children(&app, video)[0];
    click(&mut app, video_header);
    settle(&mut app);
    assert!(app.world().get::<Foldable>(video).unwrap().open);
    assert!(!app.world().get::<Foldable>(audio).unwrap().open);
    assert_ne!(display(&app, vsync), Display::None);
    assert_eq!(display(&app, volume), Display::None);

    // Tabs: the bar first, then one page per tab; only the active page shows.
    assert!(app.world().get::<TabPages>(pages).is_some());
    let tab_parts = children(&app, pages);
    let bar = tab_parts[0];
    assert_eq!(app.world().get::<TabBar>(bar).unwrap().active, 1);
    let general = named(&mut app, "general");
    let controls = named(&mut app, "controls");
    assert_eq!(app.world().get::<TabPage>(general).unwrap().index, 0);
    assert_eq!(display(&app, general), Display::None);
    assert_ne!(display(&app, controls), Display::None);
    // Clicking the first tab button switches the page.
    let first_tab = children(&app, bar)[0];
    click(&mut app, first_tab);
    settle(&mut app);
    assert_eq!(app.world().get::<TabBar>(bar).unwrap().active, 0);
    assert_ne!(display(&app, general), Display::None);
    assert_eq!(display(&app, controls), Display::None);
}

#[test]
fn markup_text_renders_through_the_markup_parser() {
    let mut app = ui_app();
    spawn_parity(&mut app);
    let title = named(&mut app, "title");
    let text = app.world().get::<Text>(title).unwrap();
    let lit: Vec<(String, bool)> = text
        .spans
        .iter()
        .map(|s| match &s.content {
            TextContent::Lit(t) => (t.clone(), s.style.bold),
            TextContent::Loc { key, .. } => (key.clone(), s.style.bold),
        })
        .collect();
    assert_eq!(
        lit,
        vec![
            ("Settings".to_owned(), true),
            (" for ".to_owned(), false),
            ("Regia".to_owned(), false),
        ]
    );
    assert_ne!(
        text.spans[2].style.color, text.spans[1].style.color,
        "<color> applies"
    );

    // Without `markup` the tags are text.
    let general = named(&mut app, "general");
    let plain = app.world().get::<Text>(general).unwrap();
    assert_eq!(plain.spans.len(), 1);
}

/// Review sprint-w12-rev-g13 #1: a neighbor named later in the same frame (after the resolve ran)
/// is found on a later frame, and a node made focusable after the first resolve gets its links.
#[test]
fn a_neighbor_spawned_after_the_resolve_in_the_same_frame_is_found_later() {
    use regia_ui::authoring::{DeclaredNav, NavDef, resolve_declared_nav};
    #[derive(Resource, Default)]
    struct Late(Option<Entity>);
    fn spawn_late(mut commands: Commands, mut late: ResMut<Late>, roots: Query<Entity, With<LateRoot>>) {
        if late.0.is_none()
            && let Some(root) = roots.iter().next()
        {
            late.0 = Some(
                commands
                    .spawn((NodeName("late".into()), Focusable::default(), ChildOf(root)))
                    .id(),
            );
        }
    }
    #[derive(Component)]
    struct LateRoot;
    let mut app = ui_app();
    app.init_resource::<Late>();
    app.add_systems(Update, spawn_late.after(resolve_declared_nav));
    let root = app.world_mut().spawn(LateRoot).id();
    let declared = DeclaredNav::new(NavDef {
        down: Some("late".into()),
        ..Default::default()
    });
    let node = app
        .world_mut()
        .spawn((declared.clone(), Focusable::default(), ChildOf(root)))
        .id();
    // Frame 1: the resolve finds nothing, then `late` spawns after it.
    app.update();
    let late = app.world().resource::<Late>().0.expect("spawned");
    // A steady screen afterwards: nothing else spawns.
    app.update();
    app.update();
    assert_eq!(
        app.world().get::<Focusable>(node).unwrap().nav.down,
        Some(late),
        "the late neighbor is linked"
    );
    // A declaration on a node that is not focusable yet stays unresolved until it is.
    let plain = app.world_mut().spawn((declared, ChildOf(root))).id();
    app.update();
    assert!(!app.world().get::<DeclaredNav>(plain).unwrap().resolved);
    app.world_mut().entity_mut(plain).insert(Focusable::default());
    app.update();
    assert_eq!(app.world().get::<Focusable>(plain).unwrap().nav.down, Some(late));
}

#[test]
fn arrow_navigation_follows_the_declared_neighbors() {
    let mut app = ui_app();
    spawn_parity(&mut app);
    let apply = named(&mut app, "apply");
    let reset = named(&mut app, "reset");
    let quit = named(&mut app, "quit");
    let links = app.world().get::<Focusable>(apply).unwrap().nav;
    assert_eq!((links.right, links.down), (Some(reset), Some(quit)));

    app.world_mut().entity_mut(apply).insert(Focused);
    app.world_mut().resource_mut::<FocusState>().focused = Some(apply);
    app.update();
    // Geometry finds nothing below the bottom row; the declared neighbor does.
    key(&mut app, "ArrowDown");
    assert_eq!(app.world().resource::<FocusState>().focused, Some(quit));
    // Geometry would go up into the settings panes; the declared neighbor goes back.
    key(&mut app, "ArrowUp");
    assert_eq!(app.world().resource::<FocusState>().focused, Some(apply));
    key(&mut app, "ArrowRight");
    assert_eq!(app.world().resource::<FocusState>().focused, Some(reset));
    key(&mut app, "ArrowLeft");
    assert_eq!(app.world().resource::<FocusState>().focused, Some(apply));
}

#[test]
fn pointer_focus_never_shows_the_focus_visible_layer() {
    let mut app = ui_app();
    spawn_parity(&mut app);
    let apply = named(&mut app, "apply");
    let quit = named(&mut app, "quit");
    let class = StyleClass {
        focused: VisualOverrides {
            border_width: Some(3.0),
            ..Default::default()
        },
        focus_visible: VisualOverrides {
            border_width: Some(7.0),
            ..Default::default()
        },
        ..Default::default()
    };
    for e in [apply, quit] {
        app.world_mut().entity_mut(e).insert(ThemeOverride(class.clone()));
    }
    settle(&mut app);

    // Pointer mode (a click on a control; a click never moves focus), then focus given in code.
    click(&mut app, apply);
    app.world_mut().entity_mut(apply).insert(Focused);
    app.world_mut().resource_mut::<FocusState>().focused = Some(apply);
    settle(&mut app);
    let t = *app.world().get::<Themed>(apply).unwrap();
    assert!(
        t.focused && !t.focus_visible,
        "focus without navigation is not visible focus"
    );
    assert_eq!(t.border_width, Some(3.0), "the focused layer only");

    key(&mut app, "ArrowDown");
    settle(&mut app);
    assert_eq!(app.world().resource::<FocusState>().focused, Some(quit));
    let t = *app.world().get::<Themed>(quit).unwrap();
    assert!(t.focused && t.focus_visible, "keyboard focus is visible");
    assert_eq!(t.border_width, Some(7.0), "focus_visible wins over focused");

    // A click keeps the focus where it is and drops the visible-focus look.
    click(&mut app, quit);
    settle(&mut app);
    assert_eq!(app.world().resource::<FocusState>().focused, Some(quit));
    let t = *app.world().get::<Themed>(quit).unwrap();
    assert!(
        t.focused && !t.focus_visible,
        "pointer input hides the visible-focus state"
    );
    assert_eq!(t.border_width, Some(3.0));
}
